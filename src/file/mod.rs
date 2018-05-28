use std::borrow::Cow;
use std::cmp;
use std::default::Default;
use std::fs;
use std::ops::Deref;

mod dwarf;

use fnv::FnvHashMap as HashMap;
use gimli;
use memmap;
use object::{self, Object, ObjectSection, ObjectSegment};
use panopticon;

use function::{Function, FunctionOffset};
use print::{DiffList, DiffState, MergeIterator, MergeResult, Print, PrintState, Printer, SortList,
            ValuePrinter};
use range::{Range, RangeList};
use types::{Type, TypeOffset};
use unit::Unit;
use variable::Variable;
use {Address, Options, Result, Size};

#[derive(Debug)]
pub(crate) struct CodeRegion {
    pub machine: panopticon::Machine,
    pub region: panopticon::Region,
}

pub(crate) trait DebugInfo {
    fn type_from_offset(&self, offset: TypeOffset) -> Option<Type>;
}

pub struct File<'input> {
    path: &'input str,
    code: Option<CodeRegion>,
    sections: Vec<Section<'input>>,
    symbols: Vec<Symbol<'input>>,
    units: Vec<Unit<'input>>,
    debug_info: &'input DebugInfo,
}

impl<'input> File<'input> {
    pub(crate) fn type_from_offset(&self, offset: TypeOffset) -> Option<Type<'input>> {
        self.debug_info.type_from_offset(offset)
    }

    pub fn parse<Cb>(path: &str, cb: Cb) -> Result<()>
    where
        Cb: FnOnce(&File) -> Result<()>,
    {
        let handle = match fs::File::open(path) {
            Ok(handle) => handle,
            Err(e) => {
                return Err(format!("open failed: {}", e).into());
            }
        };

        let map = match unsafe { memmap::Mmap::map(&handle) } {
            Ok(map) => map,
            Err(e) => {
                return Err(format!("memmap failed: {}", e).into());
            }
        };

        let input = &*map;
        /*
        if input.starts_with(b"Microsoft C/C++ MSF 7.00\r\n\x1a\x44\x53\x00") {
            pdb::parse(input, path, cb)
        } else {
            File::parse_object(input, path, cb)
        }
        */
        File::parse_object(input, path, cb)
    }

    fn parse_object<Cb>(input: &[u8], path: &str, cb: Cb) -> Result<()>
    where
        Cb: FnOnce(&File) -> Result<()>,
    {
        let object = object::File::parse(input)?;

        let machine = match object.machine() {
            object::Machine::X86_64 => {
                let region =
                    panopticon::Region::undefined("RAM".to_string(), 0xFFFF_FFFF_FFFF_FFFF);
                Some((panopticon::Machine::Amd64, region))
            }
            _ => None,
        };

        let mut code = None;
        if let Some((machine, mut region)) = machine {
            for segment in object.segments() {
                let data = segment.data();
                let address = segment.address();
                let bound = panopticon::Bound::new(address, address + data.len() as u64);
                let layer = panopticon::Layer::wrap(data.to_vec());
                region.cover(bound, layer);
            }
            code = Some(CodeRegion { machine, region });
        }

        let mut sections = Vec::new();
        for section in object.sections() {
            let name = section.name().map(|x| Cow::Owned(x.to_string()));
            let segment = section.segment_name().map(|x| Cow::Owned(x.to_string()));
            let address = if section.address() != 0 {
                Some(section.address())
            } else {
                None
            };
            let size = section.size();
            if size != 0 {
                sections.push(Section {
                    name,
                    segment,
                    address,
                    size,
                });
            }
        }

        let mut symbols = Vec::new();
        for symbol in object.symbols() {
            // TODO: handle relocatable objects
            let address = symbol.address();
            if address == 0 {
                continue;
            }

            let size = symbol.size();
            if size == 0 {
                continue;
            }

            // TODO: handle SymbolKind::File
            let ty = match symbol.kind() {
                object::SymbolKind::Text => SymbolType::Function,
                object::SymbolKind::Data | object::SymbolKind::Unknown => SymbolType::Variable,
                _ => continue,
            };

            let name = symbol.name().map(Cow::Borrowed);

            symbols.push(Symbol {
                name,
                ty,
                address,
                size,
            });
        }

        let endian = if object.is_little_endian() {
            gimli::RunTimeEndian::Little
        } else {
            gimli::RunTimeEndian::Big
        };

        dwarf::parse(endian, &object, |units, debug_info| {
            let mut file = File {
                path,
                code,
                sections,
                symbols,
                units,
                debug_info,
            };
            file.normalize();
            cb(&file)
        })
    }

    fn normalize(&mut self) {
        self.symbols.sort_by(|a, b| a.address.cmp(&b.address));
        let mut used_symbols = vec![false; self.symbols.len()];

        // Set symbol names on functions/variables.
        for unit in &mut self.units {
            for function in &mut unit.functions {
                if let Some(address) = function.address() {
                    if let Some(symbol) = Self::get_symbol(
                        &*self.symbols,
                        &mut used_symbols,
                        address,
                        function.linkage_name().or(function.name()),
                    ) {
                        function.symbol_name = symbol.name.clone();
                    }
                }
            }

            for variable in &mut unit.variables {
                if let Some(address) = variable.address() {
                    if let Some(symbol) = Self::get_symbol(
                        &*self.symbols,
                        &mut used_symbols,
                        address,
                        variable.linkage_name().or(variable.name()),
                    ) {
                        variable.symbol_name = symbol.name.clone();
                    }
                }
            }
        }

        // Create a unit for symbols that don't have debuginfo.
        let mut unit = Unit::default();
        unit.name = Some(Cow::Borrowed("<symtab>"));
        for (symbol, used) in self.symbols.iter().zip(used_symbols.iter()) {
            if *used {
                continue;
            }
            unit.ranges.push(Range {
                begin: symbol.address,
                end: symbol.address + symbol.size,
            });
            match symbol.ty {
                SymbolType::Variable => {
                    unit.variables.push(Variable {
                        name: symbol.name.clone(),
                        linkage_name: symbol.name.clone(),
                        address: Address::new(symbol.address),
                        size: Size::new(symbol.size),
                        ..Default::default()
                    });
                }
                SymbolType::Function => {
                    unit.functions.push(Function {
                        name: symbol.name.clone(),
                        linkage_name: symbol.name.clone(),
                        address: Address::new(symbol.address),
                        size: Size::new(symbol.size),
                        ..Default::default()
                    });
                }
            }
        }
        unit.ranges.sort();
        self.units.push(unit);

        // Create a unit for all remaining address ranges.
        let mut unit = Unit::default();
        unit.name = Some(Cow::Borrowed("<unknown>"));
        unit.ranges = self.unknown_ranges();
        self.units.push(unit);
    }

    // Determine if the symbol at the given address has the given name.
    // There may be multiple symbols for the same address.
    // If none match the given name, then return the first one.
    fn get_symbol<'sym>(
        symbols: &'sym [Symbol<'input>],
        used_symbols: &mut [bool],
        address: u64,
        name: Option<&str>,
    ) -> Option<&'sym Symbol<'input>> {
        if let Ok(mut index) = symbols.binary_search_by(|x| x.address.cmp(&address)) {
            while index > 0 && symbols[index - 1].address == address {
                index -= 1;
            }
            let mut found = false;
            for (symbol, used_symbol) in (&symbols[index..])
                .iter()
                .zip((&mut used_symbols[index..]).iter_mut())
            {
                if symbol.address != address {
                    break;
                }
                *used_symbol = true;
                if symbol.name() == name {
                    found = true;
                }
            }
            if found {
                None
            } else {
                Some(&symbols[index])
            }
        } else {
            None
        }
    }

    pub(crate) fn code(&self) -> Option<&CodeRegion> {
        self.code.as_ref()
    }

    fn ranges(&self, hash: &FileHash) -> RangeList {
        let mut ranges = RangeList::default();
        for unit in &self.units {
            for range in unit.ranges(hash).list() {
                ranges.push(*range);
            }
            for range in unit.unknown_ranges(hash).list() {
                ranges.push(*range);
            }
        }
        ranges.sort();
        ranges
    }

    // Used to create <unknown> unit. After creation of that unit
    // this will return an empty range list.
    fn unknown_ranges(&self) -> RangeList {
        let hash = FileHash::new(self);
        let unit_ranges = self.ranges(&hash);

        let mut ranges = RangeList::default();
        for section in &self.sections {
            if let Some(range) = section.address() {
                ranges.push(range);
            }
        }
        ranges.sort();
        ranges.subtract(&unit_ranges)
    }

    fn function_size(&self) -> u64 {
        let mut size = 0;
        for unit in &self.units {
            size += unit.function_size();
        }
        size
    }

    fn variable_size(&self, hash: &FileHash) -> u64 {
        let mut size = 0;
        for unit in &self.units {
            size += unit.variable_size(hash);
        }
        size
    }

    fn assign_ids(&self, options: &Options) {
        let mut id = 0;
        for unit in &self.units {
            id = unit.assign_ids(options, id);
        }
    }

    fn assign_merged_ids(
        hash_a: &FileHash,
        file_a: &File,
        hash_b: &FileHash,
        file_b: &File,
        options: &Options,
    ) {
        let mut id = 0;
        for unit in File::merged_units(hash_a, file_a, hash_b, file_b, options) {
            match unit {
                MergeResult::Both(a, b) => {
                    id = Unit::assign_merged_ids(hash_a, a, hash_b, b, options, id);
                }
                MergeResult::Left(a) => {
                    id = a.assign_ids(options, id);
                }
                MergeResult::Right(b) => {
                    id = b.assign_ids(options, id);
                }
            }
        }
    }

    fn merged_units<'a>(
        hash_a: &FileHash,
        file_a: &'a File<'input>,
        hash_b: &FileHash,
        file_b: &'a File<'input>,
        options: &Options,
    ) -> Vec<MergeResult<&'a Unit<'input>, &'a Unit<'input>>> {
        let mut units_a = file_a.filter_units(options);
        units_a.sort_by(|x, y| Unit::cmp_id(hash_a, x, hash_a, y, options));
        let mut units_b = file_b.filter_units(options);
        units_b.sort_by(|x, y| Unit::cmp_id(hash_b, x, hash_b, y, options));
        MergeIterator::new(units_a.into_iter(), units_b.into_iter(), |a, b| {
            Unit::cmp_id(hash_a, a, hash_b, b, options)
        }).collect()
    }

    pub fn print(&self, printer: &mut Printer, options: &Options) -> Result<()> {
        self.assign_ids(options);
        let hash = FileHash::new(self);
        let mut state = PrintState::new(printer, &hash, options);

        if options.category_file {
            state.collapsed(
                |state| {
                    state.line(|w, _hash| {
                        write!(w, "file {}", self.path)?;
                        Ok(())
                    })
                },
                |state| {
                    let ranges = self.ranges(state.hash());
                    let size = ranges.size();
                    let fn_size = self.function_size();
                    let var_size = self.variable_size(state.hash());
                    let other_size = size - fn_size - var_size;
                    if options.print_file_address {
                        state.field_collapsed("addresses", |state| state.list(&(), ranges.list()))?;
                    }
                    state.field_u64("size", size)?;
                    state.field_u64("fn size", fn_size)?;
                    state.field_u64("var size", var_size)?;
                    state.field_u64("other size", other_size)?;
                    state.field_collapsed("sections", |state| state.list(&(), &*self.sections))?;
                    Ok(())
                },
            )?;
            state.line_break()?;
        }

        state.sort_list(&(), &mut self.filter_units(options))
    }

    pub fn diff(
        printer: &mut Printer,
        file_a: &File,
        file_b: &File,
        options: &Options,
    ) -> Result<()> {
        let hash_a = FileHash::new(file_a);
        let hash_b = FileHash::new(file_b);
        File::assign_merged_ids(&hash_a, file_a, &hash_b, file_b, options);

        let mut state = DiffState::new(printer, &hash_a, &hash_b, options);

        if options.category_file {
            state.collapsed(
                |state| {
                    state.line(file_a, file_b, |w, _hash, x| {
                        write!(w, "file {}", x.path)?;
                        Ok(())
                    })
                },
                |state| {
                    let ranges_a = file_a.ranges(state.hash_a());
                    let ranges_b = file_b.ranges(state.hash_b());
                    let size_a = ranges_a.size();
                    let size_b = ranges_b.size();
                    let fn_size_a = file_a.function_size();
                    let fn_size_b = file_b.function_size();
                    let var_size_a = file_a.variable_size(state.hash_a());
                    let var_size_b = file_b.variable_size(state.hash_b());
                    let other_size_a = size_a - fn_size_a - var_size_a;
                    let other_size_b = size_b - fn_size_b - var_size_b;
                    if options.print_file_address {
                        state.field_collapsed("addresses", |state| {
                            state.ord_list(&(), ranges_a.list(), &(), ranges_b.list())
                        })?;
                    }
                    state.field_u64("size", size_a, size_b)?;
                    state.field_u64("fn size", fn_size_a, fn_size_b)?;
                    state.field_u64("var size", var_size_a, var_size_b)?;
                    state.field_u64("other size", other_size_a, other_size_b)?;
                    // TODO: sort sections
                    state.field_collapsed("sections", |state| {
                        state.list(&(), &*file_a.sections, &(), &*file_b.sections)
                    })?;
                    Ok(())
                },
            )?;
            state.line_break()?;
        }

        state.sort_list(
            &(),
            &(),
            &mut File::merged_units(&hash_a, file_a, &hash_b, file_b, options),
        )
    }

    fn filter_units(&self, options: &Options) -> Vec<&Unit<'input>> {
        self.units.iter().filter(|a| a.filter(options)).collect()
    }
}

pub(crate) struct FileHash<'input> {
    pub file: &'input File<'input>,
    // All functions by address.
    pub functions_by_address: HashMap<u64, &'input Function<'input>>,
    // All functions by offset.
    pub functions_by_offset: HashMap<FunctionOffset, &'input Function<'input>>,
    // All types by offset.
    pub types: HashMap<TypeOffset, &'input Type<'input>>,
}

impl<'input> FileHash<'input> {
    fn new(file: &'input File<'input>) -> Self {
        FileHash {
            file,
            functions_by_address: FileHash::functions_by_address(file),
            functions_by_offset: FileHash::functions_by_offset(file),
            types: FileHash::types(file),
        }
    }

    /// Returns a map from address to function for all functions in the file.
    fn functions_by_address<'a>(file: &'a File<'input>) -> HashMap<u64, &'a Function<'input>> {
        let mut functions = HashMap::default();
        for unit in &file.units {
            for function in &unit.functions {
                if let Some(address) = function.address() {
                    // TODO: handle duplicate addresses
                    functions.insert(address, function);
                }
            }
        }
        functions
    }

    /// Returns a map from offset to function for all functions in the file.
    fn functions_by_offset<'a>(
        file: &'a File<'input>,
    ) -> HashMap<FunctionOffset, &'a Function<'input>> {
        let mut functions = HashMap::default();
        for unit in &file.units {
            for function in &unit.functions {
                functions.insert(function.offset, function);
            }
        }
        functions
    }

    /// Returns a map from offset to type for all types in the file.
    fn types<'a>(file: &'a File<'input>) -> HashMap<TypeOffset, &'a Type<'input>> {
        let mut types = HashMap::default();
        for unit in &file.units {
            for ty in &unit.types {
                types.insert(ty.offset, ty);
            }
        }
        types
    }
}

#[derive(Debug)]
pub(crate) struct Section<'input> {
    name: Option<Cow<'input, str>>,
    segment: Option<Cow<'input, str>>,
    address: Option<u64>,
    size: u64,
}

impl<'input> Section<'input> {
    fn name(&self) -> Option<&str> {
        self.name.as_ref().map(Cow::deref)
    }

    fn segment(&self) -> Option<&str> {
        self.segment.as_ref().map(Cow::deref)
    }

    fn address(&self) -> Option<Range> {
        self.address.map(|address| Range {
            begin: address,
            end: address + self.size,
        })
    }

    fn print_name(&self, w: &mut ValuePrinter) -> Result<()> {
        if let Some(ref segment) = self.segment() {
            write!(w, "{},", segment)?;
        }
        match self.name() {
            Some(name) => write!(w, "{}", name)?,
            None => write!(w, "<anon-section>")?,
        }
        Ok(())
    }

    fn print_address(&self, w: &mut ValuePrinter) -> Result<()> {
        if let Some(address) = self.address() {
            address.print_address(w)?;
        }
        Ok(())
    }
}

impl<'input> Print for Section<'input> {
    type Arg = ();

    fn print(&self, state: &mut PrintState, _arg: &()) -> Result<()> {
        state.collapsed(
            |state| state.line(|w, _state| self.print_name(w)),
            |state| {
                state.field("address", |w, _state| self.print_address(w))?;
                state.field_u64("size", self.size)
            },
        )
    }

    fn diff(state: &mut DiffState, _arg_a: &(), a: &Self, _arg_b: &(), b: &Self) -> Result<()> {
        state.collapsed(
            |state| state.line(a, b, |w, _state, x| x.print_name(w)),
            |state| {
                state.field("address", a, b, |w, _state, x| x.print_address(w))?;
                state.field_u64("size", a.size, b.size)
            },
        )
    }
}

impl<'input> DiffList for Section<'input> {
    fn step_cost(&self, _state: &DiffState, _arg: &()) -> usize {
        1
    }

    fn diff_cost(_state: &DiffState, _arg_a: &(), a: &Self, _arg_b: &(), b: &Self) -> usize {
        let mut cost = 0;
        if a.name.cmp(&b.name) != cmp::Ordering::Equal
            || a.segment.cmp(&b.segment) != cmp::Ordering::Equal
        {
            cost += 2;
        }
        cost
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SymbolType {
    Variable,
    Function,
}

#[derive(Debug, Clone)]
pub(crate) struct Symbol<'input> {
    name: Option<Cow<'input, str>>,
    ty: SymbolType,
    address: u64,
    size: u64,
}

impl<'input> Symbol<'input> {
    fn name(&self) -> Option<&str> {
        self.name.as_ref().map(Cow::deref)
    }

    fn address(&self) -> Range {
        Range {
            begin: self.address,
            end: self.address + self.size,
        }
    }

    fn print_name(&self, w: &mut ValuePrinter) -> Result<()> {
        match self.ty {
            SymbolType::Variable => write!(w, "var ")?,
            SymbolType::Function => write!(w, "fn ")?,
        }
        match self.name() {
            Some(name) => write!(w, "{}", name)?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn print_address(&self, w: &mut ValuePrinter) -> Result<()> {
        self.address().print_address(w)?;
        Ok(())
    }
}

impl<'input> Print for Symbol<'input> {
    type Arg = ();

    fn print(&self, state: &mut PrintState, _arg: &()) -> Result<()> {
        state.collapsed(
            |state| state.line(|w, _state| self.print_name(w)),
            |state| {
                state.field("address", |w, _state| self.print_address(w))?;
                state.field_u64("size", self.size)
            },
        )
    }

    fn diff(state: &mut DiffState, _arg_a: &(), a: &Self, _arg_b: &(), b: &Self) -> Result<()> {
        state.collapsed(
            |state| state.line(a, b, |w, _state, x| x.print_name(w)),
            |state| {
                state.field("address", a, b, |w, _state, x| x.print_address(w))?;
                state.field_u64("size", a.size, b.size)
            },
        )
    }
}

impl<'input> DiffList for Symbol<'input> {
    fn step_cost(&self, _state: &DiffState, _arg: &()) -> usize {
        1
    }

    fn diff_cost(_state: &DiffState, _arg_a: &(), a: &Self, _arg_b: &(), b: &Self) -> usize {
        let mut cost = 0;
        if a.name.cmp(&b.name) != cmp::Ordering::Equal {
            cost += 2;
        }
        cost
    }
}
