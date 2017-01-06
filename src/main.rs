extern crate env_logger;
extern crate getopts;
extern crate gimli;
#[macro_use]
extern crate log;
extern crate memmap;
extern crate xmas_elf;
extern crate panopticon;

use std::borrow::Borrow;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::cmp;
use std::env;
use std::error;
use std::fmt::{self, Debug};
use std::fs;
use std::ffi;
use std::io::{self, Write};
use std::result;
use std::rc::Rc;

use panopticon::amd64;

#[derive(Debug)]
pub struct Error(pub Cow<'static, str>);

impl error::Error for Error {
    fn description(&self) -> &str {
        self.0.borrow()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&'static str> for Error {
    fn from(s: &'static str) -> Error {
        Error(Cow::Borrowed(s))
    }
}

impl From<String> for Error {
    fn from(s: String) -> Error {
        Error(Cow::Owned(s))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error(Cow::Owned(format!("IO error: {}", e)))
    }
}

impl From<gimli::Error> for Error {
    fn from(e: gimli::Error) -> Error {
        Error(Cow::Owned(format!("DWARF error: {}", e)))
    }
}

pub type Result<T> = result::Result<T, Error>;

fn main() {
    env_logger::init().ok();

    let mut opts = getopts::Options::new();
    opts.optflag("", "calls", "print subprogram calls");
    opts.optflag("", "diff", "print difference between two files");
    opts.optflag("", "sort", "sort entries by type and name");
    opts.optopt("",
                "inline-depth",
                "depth of inlined subroutine calls (0 to disable)",
                "DEPTH");
    opts.optopt("", "name", "print only members with the given name", "NAME");
    opts.optopt("",
                "namespace",
                "print only members of the given namespace",
                "NAMESPACE");

    let matches = match opts.parse(env::args().skip(1)) {
        Ok(m) => m,
        Err(e) => {
            error!("{}", e);
            print_usage(&opts);
        }
    };

    let calls = matches.opt_present("calls");
    let diff = matches.opt_present("diff");
    let sort = matches.opt_present("sort");
    let inline_depth = if let Some(inline_depth) = matches.opt_str("inline-depth") {
        match inline_depth.parse::<usize>() {
            Ok(inline_depth) => inline_depth,
            Err(e) => {
                error!("Invalid argument '{}' to option 'inline-depth': {}",
                       inline_depth,
                       e);
                print_usage(&opts);
            }
        }
    } else {
        1
    };
    let name = matches.opt_str("name");
    let name = name.as_ref().map(|s| &s[..]);
    let namespace = matches.opt_str("namespace");
    let namespace = match namespace {
        Some(ref namespace) => namespace.split("::").collect(),
        None => Vec::new(),
    };

    let flags = Flags {
        calls: calls,
        diff: diff,
        sort: sort,
        inline_depth: inline_depth,
        name: name,
        namespace: namespace,
    };

    if flags.diff {
        if matches.free.len() != 2 {
            error!("Invalid filename arguments (expected 2 filenames, found {})",
                   matches.free.len());
            print_usage(&opts);
        }
        let path_a = &matches.free[0];
        let path_b = &matches.free[1];

        if let Err(e) = parse_file(path_a,
                                   &mut |file_a| {
            if let Err(e) = parse_file(path_b,
                                       &mut |file_b| {
                                           if let Err(e) = diff_file(file_a, file_b, &flags) {
                                               error!("{}", e);
                                           }
                                           Ok(())
                                       }) {
                error!("{}: {}", path_b, e);
            }
            Ok(())
        }) {
            error!("{}: {}", path_a, e);
        }
    } else {
        if matches.free.len() != 1 {
            error!("Invalid filename arguments (expected 1 filename, found {})",
                   matches.free.len());
            print_usage(&opts);
        }
        let path = &matches.free[0];

        if let Err(e) = parse_file(path, &mut |file| print_file(file, &flags)) {
            error!("{}: {}", path, e);
        }
    }
}

fn print_usage(opts: &getopts::Options) -> ! {
    let brief = format!("Usage: {} <options> <file>", env::args().next().unwrap());
    write!(&mut io::stderr(), "{}", opts.usage(&brief)).ok();
    std::process::exit(1);
}

struct Flags<'a> {
    calls: bool,
    diff: bool,
    sort: bool,
    inline_depth: usize,
    name: Option<&'a str>,
    namespace: Vec<&'a str>,
}

#[derive(Debug)]
struct File<'input> {
    // TODO: use format independent machine type
    machine: xmas_elf::header::Machine,
    region: panopticon::Region,
    units: Vec<Unit<'input>>,
}

impl<'input> File<'input> {
    fn sort(&mut self) {
        self.units.sort_by(Unit::cmp);
        for unit in &mut self.units {
            unit.sort();
        }
    }
}

fn parse_file(path: &str, cb: &mut FnMut(&mut File) -> Result<()>) -> Result<()> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) => {
            return Err(format!("open failed: {}", e).into());
        }
    };

    let file = match memmap::Mmap::open(&file, memmap::Protection::Read) {
        Ok(file) => file,
        Err(e) => {
            return Err(format!("memmap failed: {}", e).into());
        }
    };

    let input = unsafe { file.as_slice() };
    let elf = xmas_elf::ElfFile::new(input);
    let machine = elf.header.pt2?.machine();
    let mut region = match machine {
        xmas_elf::header::Machine::X86_64 => {
            panopticon::Region::undefined("RAM".to_string(), 0xFFFF_FFFF_FFFF_FFFF)
        }
        machine => return Err(format!("Unsupported machine: {:?}", machine).into()),
    };

    for ph in elf.program_iter() {
        if ph.get_type() == Ok(xmas_elf::program::Type::Load) {
            let offset = ph.offset();
            let size = ph.file_size();
            let addr = ph.virtual_addr();
            if offset as usize <= elf.input.len() {
                let input = &elf.input[offset as usize..];
                if size as usize <= input.len() {
                    let bound = panopticon::Bound::new(addr, addr + size);
                    let layer = panopticon::Layer::wrap(input[..size as usize].to_vec());
                    region.cover(bound, layer);
                    debug!("loaded program header addr {:#x} size {:#x}", addr, size);
                } else {
                    debug!("invalid program header size {}", size);
                }
            } else {
                debug!("invalid program header offset {}", offset);
            }
        }
    }

    let units = match elf.header.pt1.data {
        xmas_elf::header::Data::LittleEndian => parse_dwarf::<gimli::LittleEndian>(&elf)?,
        xmas_elf::header::Data::BigEndian => parse_dwarf::<gimli::BigEndian>(&elf)?,
        _ => {
            return Err("Unknown endianity".into());
        }
    };

    let mut file = File {
        machine: machine,
        region: region,
        units: units,
    };

    cb(&mut file)
}

struct PrintState<'a, 'input>
    where 'input: 'a
{
    file: &'a File<'input>,
    flags: &'a Flags<'a>,
    // All subprograms by address.
    all_subprograms: HashMap<u64, &'a Subprogram<'input>>,
    // Unit subprograms by offset.
    subprograms: HashMap<usize, &'a Subprogram<'input>>,
    // Unit types by offset.
    types: HashMap<usize, &'a Type<'input>>,
    address_size: Option<u64>,
    indent: usize,
    prefix: &'static str,
}

impl<'a, 'input> PrintState<'a, 'input>
    where 'input: 'a
{
    fn new(file: &'a File<'input>, flags: &'a Flags<'a>) -> Self {
        let mut state = PrintState {
            file: file,
            flags: flags,
            all_subprograms: HashMap::new(),
            subprograms: HashMap::new(),
            types: HashMap::new(),
            address_size: None,
            indent: 0,
            prefix: "",
        };

        // TODO: insert symbol table names too
        for unit in &file.units {
            for ty in &unit.types {
                match ty.kind {
                    TypeKind::Struct(StructType { ref subprograms, .. }) |
                    TypeKind::Union(UnionType { ref subprograms, .. }) |
                    TypeKind::Enumeration(EnumerationType { ref subprograms, .. }) => {
                        for subprogram in subprograms {
                            if let Some(low_pc) = subprogram.low_pc {
                                state.all_subprograms.insert(low_pc, subprogram);
                            }
                        }
                    }
                    TypeKind::Base(_) |
                    TypeKind::Def(_) |
                    TypeKind::Array(_) |
                    TypeKind::Subroutine(_) |
                    TypeKind::Modifier(_) |
                    TypeKind::Unimplemented(_) => {}
                }
            }
            for subprogram in &unit.subprograms {
                if let Some(low_pc) = subprogram.low_pc {
                    state.all_subprograms.insert(low_pc, subprogram);
                }
            }
        }
        state
    }

    fn set_unit(&mut self, unit: &'a Unit<'input>) {
        self.types.clear();
        self.subprograms.clear();
        for ty in &unit.types {
            self.types.insert(ty.offset.0, ty);
            match ty.kind {
                TypeKind::Struct(StructType { ref subprograms, .. }) |
                TypeKind::Union(UnionType { ref subprograms, .. }) |
                TypeKind::Enumeration(EnumerationType { ref subprograms, .. }) => {
                    for subprogram in subprograms {
                        self.subprograms.insert(subprogram.offset.0, subprogram);
                    }
                }
                TypeKind::Base(_) |
                TypeKind::Def(_) |
                TypeKind::Array(_) |
                TypeKind::Subroutine(_) |
                TypeKind::Modifier(_) |
                TypeKind::Unimplemented(_) => {}
            }
        }
        for subprogram in &unit.subprograms {
            self.subprograms.insert(subprogram.offset.0, subprogram);
        }
        self.address_size = unit.address_size;
    }

    fn filter_name(&self, name: Option<&ffi::CStr>) -> bool {
        if let Some(filter) = self.flags.name {
            filter_name(name, filter)
        } else {
            true
        }
    }

    fn filter_namespace(&self, namespace: &Namespace) -> bool {
        namespace.filter(&self.flags.namespace)
    }

    fn indent<F>(&mut self, mut f: F) -> Result<()>
        where F: FnMut(&mut PrintState) -> Result<()>
    {
        self.indent += 1;
        let ret = f(self);
        self.indent -= 1;
        ret
    }

    fn prefix<F>(&mut self, prefix: &'static str, mut f: F) -> Result<()>
        where F: FnMut(&mut PrintState) -> Result<()>
    {
        let prev = self.prefix;
        self.prefix = prefix;
        let ret = f(self);
        self.prefix = prev;
        ret
    }

    fn line_start(&self, w: &mut Write) -> Result<()> {
        write!(w, "{}", self.prefix)?;
        for _ in 0..self.indent {
            write!(w, "\t")?;
        }
        Ok(())
    }
}

fn filter_name(name: Option<&ffi::CStr>, filter: &str) -> bool {
    match name {
        Some(name) => name.to_bytes() == filter.as_bytes(),
        None => false,
    }
}

fn print_file(file: &mut File, flags: &Flags) -> Result<()> {
    if flags.sort {
        file.sort();
    }

    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut state = PrintState::new(file, flags);
    for unit in &file.units {
        state.set_unit(unit);
        unit.print(&mut writer, &mut state)?;
    }
    Ok(())
}

struct DiffState<'a, 'input>
    where 'input: 'a
{
    a: PrintState<'a, 'input>,
    b: PrintState<'a, 'input>,
}

impl<'a, 'input> DiffState<'a, 'input>
    where 'input: 'a
{
    fn new(file_a: &'a File<'input>, file_b: &'a File<'input>, flags: &'a Flags<'a>) -> Self {
        DiffState {
            a: PrintState::new(file_a, flags),
            b: PrintState::new(file_b, flags),
        }
    }

    fn merge<T, FCmp, FEqual, FLess, FGreater>(
        &mut self,
        w: &mut Write,
        iter_a: &mut Iterator<Item = T>,
        iter_b: &mut Iterator<Item = T>,
        cmp: FCmp,
        mut equal: FEqual,
        less: FLess,
        greater: FGreater
    ) -> Result<()>
        where T: Copy,
              FCmp: Fn(T, T) -> cmp::Ordering,
              FEqual: FnMut(&mut Write, T, T, &mut DiffState<'a, 'input>) -> Result<()>,
              FLess: Fn(&mut Write, T, &mut PrintState<'a, 'input>) -> Result<()>,
              FGreater: Fn(&mut Write, T, &mut PrintState<'a, 'input>) -> Result<()>
    {
        let mut item_a = iter_a.next();
        let mut item_b = iter_b.next();
        loop {
            match (item_a, item_b) {
                (Some(a), Some(b)) => {
                    match cmp(a, b) {
                        cmp::Ordering::Equal => {
                            equal(w, a, b, self)?;
                            item_a = iter_a.next();
                            item_b = iter_b.next();
                        }
                        cmp::Ordering::Less => {
                            less(w, a, &mut self.a)?;
                            item_a = iter_a.next();
                        }
                        cmp::Ordering::Greater => {
                            greater(w, b, &mut self.b)?;
                            item_b = iter_b.next();
                        }
                    }
                }
                (Some(a), None) => {
                    less(w, a, &mut self.a)?;
                    item_a = iter_a.next();
                }
                (None, Some(b)) => {
                    greater(w, b, &mut self.b)?;
                    item_b = iter_b.next();
                }
                (None, None) => break,
            };
        }
        Ok(())
    }
}

fn diff_file(file_a: &mut File, file_b: &mut File, flags: &Flags) -> Result<()> {
    file_a.sort();
    file_b.sort();

    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut state = DiffState::new(file_a, file_b, flags);
    state.merge(&mut writer,
               &mut file_a.units.iter(),
               &mut file_b.units.iter(),
               Unit::cmp,
               |w, a, b, state| {
                   state.a.set_unit(a);
                   state.b.set_unit(b);
                   Unit::diff(w, a, b, state)
               },
               |w, a, _state| {
                   writeln!(w, "First only: {:?}", a.name)?;
                   Ok(())
               },
               |w, b, _state| {
                   writeln!(w, "Second only: {:?}", b.name)?;
                   Ok(())
               })?;
    Ok(())
}

struct DwarfFileState<'input, Endian>
    where Endian: gimli::Endianity
{
    debug_abbrev: gimli::DebugAbbrev<'input, Endian>,
    debug_info: gimli::DebugInfo<'input, Endian>,
    debug_str: gimli::DebugStr<'input, Endian>,
    debug_ranges: gimli::DebugRanges<'input, Endian>,
}

fn parse_dwarf<'input, Endian>(elf: &xmas_elf::ElfFile<'input>) -> Result<Vec<Unit<'input>>>
    where Endian: gimli::Endianity
{
    let debug_abbrev = elf.find_section_by_name(".debug_abbrev").map(|s| s.raw_data(elf));
    let debug_abbrev = gimli::DebugAbbrev::<Endian>::new(debug_abbrev.unwrap_or(&[]));
    let debug_info = elf.find_section_by_name(".debug_info").map(|s| s.raw_data(elf));
    let debug_info = gimli::DebugInfo::<Endian>::new(debug_info.unwrap_or(&[]));
    let debug_str = elf.find_section_by_name(".debug_str").map(|s| s.raw_data(elf));
    let debug_str = gimli::DebugStr::<Endian>::new(debug_str.unwrap_or(&[]));
    let debug_ranges = elf.find_section_by_name(".debug_ranges").map(|s| s.raw_data(elf));
    let debug_ranges = gimli::DebugRanges::<Endian>::new(debug_ranges.unwrap_or(&[]));

    let dwarf = DwarfFileState {
        debug_abbrev: debug_abbrev,
        debug_info: debug_info,
        debug_str: debug_str,
        debug_ranges: debug_ranges,
    };

    let mut units = Vec::new();
    let mut unit_headers = dwarf.debug_info.units();
    while let Some(unit_header) = unit_headers.next()? {
        units.push(Unit::parse_dwarf(&dwarf, &unit_header)?);
    }
    Ok(units)
}

struct DwarfUnitState<'state, 'input, Endian>
    where Endian: 'state + gimli::Endianity,
          'input: 'state
{
    header: &'state gimli::CompilationUnitHeader<'input, Endian>,
    _abbrev: &'state gimli::Abbreviations,
    line: Option<gimli::DebugLineOffset>,
    ranges: Option<gimli::DebugRangesOffset>,
}

#[derive(Debug, Default)]
struct Namespace<'input> {
    parent: Option<Rc<Namespace<'input>>>,
    name: Option<&'input ffi::CStr>,
}

impl Namespace<'static> {
    fn root() -> Rc<Namespace<'static>> {
        Rc::new(Namespace {
            parent: None,
            name: None,
        })
    }
}

impl<'input> Namespace<'input> {
    fn new(
        parent: &Rc<Namespace<'input>>,
        name: Option<&'input ffi::CStr>
    ) -> Rc<Namespace<'input>> {
        Rc::new(Namespace {
            parent: Some(parent.clone()),
            name: name,
        })
    }

    fn len(&self) -> usize {
        match self.parent {
            Some(ref parent) => parent.len() + 1,
            None => 1,
        }
    }

    fn up(&self, len: usize) -> &Namespace {
        if len == 0 {
            self
        } else {
            match self.parent {
                Some(ref parent) => parent.up(len - 1),
                None => self,
            }
        }
    }

    fn print(&self, w: &mut Write) -> Result<()> {
        if let Some(ref parent) = self.parent {
            parent.print(w)?;
            match self.name {
                Some(name) => write!(w, "{}", name.to_string_lossy())?,
                None => write!(w, "<anon>")?,
            }
            write!(w, "::")?;
        }
        Ok(())
    }

    fn _filter(&self, namespace: &[&str]) -> (bool, usize) {
        match self.parent {
            Some(ref parent) => {
                let (ret, offset) = parent._filter(namespace);
                if ret {
                    if offset < namespace.len() {
                        (filter_name(self.name, namespace[offset]), offset + 1)
                    } else {
                        (true, offset)
                    }
                } else {
                    (false, 0)
                }
            }
            None => (true, 0),
        }
    }

    fn filter(&self, namespace: &[&str]) -> bool {
        self._filter(namespace) == (true, namespace.len())
    }

    fn _cmp(a: &Namespace, b: &Namespace) -> cmp::Ordering {
        debug_assert!(a.len() == b.len());
        match (a.parent.as_ref(), b.parent.as_ref()) {
            (Some(p1), Some(p2)) => {
                match Self::_cmp(p1, p2) {
                    cmp::Ordering::Equal => a.name.cmp(&b.name),
                    o => o,
                }
            }
            _ => cmp::Ordering::Equal,
        }
    }

    fn cmp(a: &Namespace, b: &Namespace) -> cmp::Ordering {
        let len_a = a.len();
        let len_b = b.len();
        match len_a.cmp(&len_b) {
            cmp::Ordering::Equal => Self::_cmp(a, b),
            cmp::Ordering::Less => {
                let b = b.up(len_b - len_a);
                match Self::_cmp(a, b) {
                    cmp::Ordering::Equal => cmp::Ordering::Less,
                    other => other,
                }
            }
            cmp::Ordering::Greater => {
                let a = a.up(len_a - len_b);
                match Self::_cmp(a, b) {
                    cmp::Ordering::Equal => cmp::Ordering::Greater,
                    other => other,
                }
            }
        }
    }
}

fn cmp_ns_and_name(
    ns1: &Namespace,
    name1: Option<&ffi::CStr>,
    ns2: &Namespace,
    name2: Option<&ffi::CStr>
) -> cmp::Ordering {
    match Namespace::cmp(ns1, ns2) {
        cmp::Ordering::Equal => name1.cmp(&name2),
        o => o,
    }
}

#[derive(Debug, Default)]
struct Unit<'input> {
    dir: Option<&'input ffi::CStr>,
    name: Option<&'input ffi::CStr>,
    language: Option<gimli::DwLang>,
    address_size: Option<u64>,
    low_pc: Option<u64>,
    high_pc: Option<u64>,
    size: Option<u64>,
    types: Vec<Type<'input>>,
    subprograms: Vec<Subprogram<'input>>,
    variables: Vec<Variable<'input>>,
}

impl<'input> Unit<'input> {
    fn parse_dwarf<Endian>(
        dwarf: &DwarfFileState<'input, Endian>,
        unit_header: &gimli::CompilationUnitHeader<'input, Endian>
    ) -> Result<Unit<'input>>
        where Endian: gimli::Endianity
    {
        let abbrev = &unit_header.abbreviations(dwarf.debug_abbrev)?;
        let mut unit_state = DwarfUnitState {
            header: unit_header,
            _abbrev: abbrev,
            line: None,
            ranges: None,
        };

        let mut tree = unit_header.entries_tree(abbrev, None)?;
        let iter = tree.iter();

        let mut unit = Unit::default();
        unit.address_size = Some(unit_header.address_size() as u64);
        if let Some(entry) = iter.entry() {
            if entry.tag() != gimli::DW_TAG_compile_unit {
                return Err(format!("unknown CU tag: {}", entry.tag()).into());
            }
            let mut attrs = entry.attrs();
            while let Some(attr) = attrs.next()? {
                match attr.name() {
                    gimli::DW_AT_name => unit.name = attr.string_value(&dwarf.debug_str),
                    gimli::DW_AT_comp_dir => unit.dir = attr.string_value(&dwarf.debug_str),
                    gimli::DW_AT_language => {
                        if let gimli::AttributeValue::Language(language) = attr.value() {
                            unit.language = Some(language);
                        }
                    }
                    gimli::DW_AT_low_pc => {
                        if let gimli::AttributeValue::Addr(addr) = attr.value() {
                            unit.low_pc = Some(addr);
                        }
                    }
                    gimli::DW_AT_high_pc => {
                        match attr.value() {
                            gimli::AttributeValue::Addr(addr) => unit.high_pc = Some(addr),
                            gimli::AttributeValue::Udata(size) => unit.size = Some(size),
                            _ => {}
                        }
                    }
                    gimli::DW_AT_stmt_list => {
                        if let gimli::AttributeValue::DebugLineRef(line) = attr.value() {
                            unit_state.line = Some(line);
                        }
                    }
                    gimli::DW_AT_ranges => {
                        if let gimli::AttributeValue::DebugRangesRef(ranges) = attr.value() {
                            unit_state.ranges = Some(ranges);
                        }
                    }
                    gimli::DW_AT_producer |
                    gimli::DW_AT_entry_pc => {}
                    _ => debug!("unknown CU attribute: {} {:?}", attr.name(), attr.value()),
                }
            }
            debug!("{:?}", unit);
        } else {
            return Err("missing CU entry".into());
        };

        let namespace = Namespace::root();
        unit.parse_dwarf_children(dwarf, &mut unit_state, &namespace, iter)?;
        Ok(unit)
    }

    fn parse_dwarf_children<'state, 'abbrev, 'unit, 'tree, Endian>(
        &mut self,
        dwarf: &DwarfFileState<'input, Endian>,
        unit: &mut DwarfUnitState<'state, 'input, Endian>,
        namespace: &Rc<Namespace<'input>>,
        mut iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
    ) -> Result<()>
        where Endian: gimli::Endianity
    {
        while let Some(child) = iter.next()? {
            match child.entry().unwrap().tag() {
                gimli::DW_TAG_namespace => {
                    self.parse_dwarf_namespace(dwarf, unit, namespace, child)?;
                }
                gimli::DW_TAG_subprogram => {
                    self.subprograms
                        .push(Subprogram::parse_dwarf(dwarf, unit, namespace, child)?);
                }
                gimli::DW_TAG_variable => {
                    self.variables.push(Variable::parse_dwarf(dwarf, unit, namespace, child)?);
                }
                gimli::DW_TAG_base_type |
                gimli::DW_TAG_structure_type |
                gimli::DW_TAG_union_type |
                gimli::DW_TAG_enumeration_type |
                gimli::DW_TAG_array_type |
                gimli::DW_TAG_subroutine_type |
                gimli::DW_TAG_typedef |
                gimli::DW_TAG_const_type |
                gimli::DW_TAG_pointer_type |
                gimli::DW_TAG_restrict_type => {
                    self.types.push(Type::parse_dwarf(dwarf, unit, namespace, child)?);
                }
                tag => {
                    debug!("unknown namespace child tag: {}", tag);
                }
            }
        }
        Ok(())
    }

    fn parse_dwarf_namespace<'state, 'abbrev, 'unit, 'tree, Endian>(
        &mut self,
        dwarf: &DwarfFileState<'input, Endian>,
        unit: &mut DwarfUnitState<'state, 'input, Endian>,
        namespace: &Rc<Namespace<'input>>,
        iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
    ) -> Result<()>
        where Endian: gimli::Endianity
    {
        let mut name = None;

        {
            let entry = iter.entry().unwrap();
            let mut attrs = entry.attrs();
            while let Some(attr) = attrs.next()? {
                match attr.name() {
                    gimli::DW_AT_name => {
                        name = attr.string_value(&dwarf.debug_str);
                    }
                    gimli::DW_AT_decl_file |
                    gimli::DW_AT_decl_line => {}
                    _ => {
                        debug!("unknown namespace attribute: {} {:?}",
                               attr.name(),
                               attr.value())
                    }
                }
            }
        }

        let namespace = Namespace::new(namespace, name);
        self.parse_dwarf_children(dwarf, unit, &namespace, iter)
    }

    fn print(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        let inline_types = self.inline_types(state);

        for ty in &self.types {
            if !inline_types.contains(&ty.offset.0) {
                ty.print(w, state)?;
            }
        }
        for subprogram in &self.subprograms {
            subprogram.print(w, state)?;
        }
        for variable in &self.variables {
            variable.print(w, state)?;
        }
        Ok(())
    }

    // The offsets of types that should be printed inline.
    fn inline_types(&self, state: &PrintState) -> HashSet<usize> {
        let mut inline_types = HashSet::new();
        for ty in &self.types {
            // Assume all anonymous types are inline. We don't actually check
            // that they will be inline, but in future we could (eg for TypeDefs).
            // TODO: is this a valid assumption?
            if ty.is_anon() {
                inline_types.insert(ty.offset.0);
            }

            // Find all inline members.
            ty.visit_members(&mut |t| {
                if t.is_inline(state) {
                    if let Some(offset) = t.ty {
                        inline_types.insert(offset.0);
                    }
                }
            });
        }
        inline_types
    }

    fn sort(&mut self) {
        self.types.sort_by(Type::cmp);
        self.subprograms.sort_by(Subprogram::cmp);
        self.variables.sort_by(Variable::cmp);

        for ty in &mut self.types {
            match ty.kind {
                TypeKind::Struct(StructType { ref mut subprograms, .. }) |
                TypeKind::Union(UnionType { ref mut subprograms, .. }) |
                TypeKind::Enumeration(EnumerationType { ref mut subprograms, .. }) => {
                    subprograms.sort_by(Subprogram::cmp);
                }
                TypeKind::Base(_) |
                TypeKind::Def(_) |
                TypeKind::Array(_) |
                TypeKind::Subroutine(_) |
                TypeKind::Modifier(_) |
                TypeKind::Unimplemented(_) => {}
            }
        }
    }

    fn cmp(a: &Unit, b: &Unit) -> cmp::Ordering {
        // TODO: ignore base paths
        a.name.cmp(&b.name)
    }

    fn diff(w: &mut Write, unit_a: &Unit, unit_b: &Unit, state: &mut DiffState) -> Result<()> {
        writeln!(w, "Both: {:?} {:?}", unit_a.name, unit_a.name)?;
        let inline_a = unit_a.inline_types(&state.a);
        let inline_b = unit_b.inline_types(&state.b);
        state.merge(w,
                   &mut unit_a.types.iter().filter(|a| !inline_a.contains(&a.offset.0)),
                   &mut unit_b.types.iter().filter(|b| !inline_b.contains(&b.offset.0)),
                   Type::cmp,
                   Type::diff,
                   |w, a, state| state.prefix("- ", |state| a.print(w, state)),
                   |w, b, state| state.prefix("+ ", |state| b.print(w, state)))?;
        state.merge(w,
                   &mut unit_a.subprograms.iter(),
                   &mut unit_b.subprograms.iter(),
                   Subprogram::cmp,
                   Subprogram::diff,
                   |w, a, state| state.prefix("- ", |state| a.print(w, state)),
                   |w, b, state| state.prefix("+ ", |state| b.print(w, state)))?;
        state.merge(w,
                   &mut unit_a.variables.iter(),
                   &mut unit_b.variables.iter(),
                   Variable::cmp,
                   Variable::diff,
                   |w, a, state| state.prefix("- ", |state| a.print(w, state)),
                   |w, b, state| state.prefix("+ ", |state| b.print(w, state)))?;
        Ok(())
    }
}

#[derive(Debug)]
enum TypeKind<'input> {
    Base(BaseType<'input>),
    Def(TypeDef<'input>),
    Struct(StructType<'input>),
    Union(UnionType<'input>),
    Enumeration(EnumerationType<'input>),
    Array(ArrayType<'input>),
    Subroutine(SubroutineType<'input>),
    Modifier(TypeModifier<'input>),
    Unimplemented(gimli::DwTag),
}

impl<'input> TypeKind<'input> {
    fn discriminant_value(&self) -> u8 {
        match *self {
            TypeKind::Base(..) => 0,
            TypeKind::Def(..) => 1,
            TypeKind::Struct(..) => 2,
            TypeKind::Union(..) => 3,
            TypeKind::Enumeration(..) => 4,
            TypeKind::Array(..) => 5,
            TypeKind::Subroutine(..) => 6,
            TypeKind::Modifier(..) => 7,
            TypeKind::Unimplemented(..) => 8,
        }
    }
}

#[derive(Debug)]
struct Type<'input> {
    offset: gimli::UnitOffset,
    kind: TypeKind<'input>,
}

impl<'input> Default for Type<'input> {
    fn default() -> Self {
        Type {
            offset: gimli::UnitOffset(0),
            kind: TypeKind::Unimplemented(gimli::DwTag(0)),
        }
    }
}

impl<'input> Type<'input> {
    fn parse_dwarf<'state, 'abbrev, 'unit, 'tree, Endian>(
        dwarf: &DwarfFileState<'input, Endian>,
        unit: &mut DwarfUnitState<'state, 'input, Endian>,
        namespace: &Rc<Namespace<'input>>,
        iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
    ) -> Result<Type<'input>>
        where Endian: gimli::Endianity
    {
        let tag = iter.entry().unwrap().tag();
        let mut ty = Type::default();
        ty.offset = iter.entry().unwrap().offset();
        ty.kind = match tag {
            gimli::DW_TAG_base_type => {
                TypeKind::Base(BaseType::parse_dwarf(dwarf, unit, namespace, iter)?)
            }
            gimli::DW_TAG_typedef => {
                TypeKind::Def(TypeDef::parse_dwarf(dwarf, unit, namespace, iter)?)
            }
            gimli::DW_TAG_structure_type => {
                TypeKind::Struct(StructType::parse_dwarf(dwarf, unit, namespace, iter)?)
            }
            gimli::DW_TAG_union_type => {
                TypeKind::Union(UnionType::parse_dwarf(dwarf, unit, namespace, iter)?)
            }
            gimli::DW_TAG_enumeration_type => {
                TypeKind::Enumeration(EnumerationType::parse_dwarf(dwarf, unit, namespace, iter)?)
            }
            gimli::DW_TAG_array_type => {
                TypeKind::Array(ArrayType::parse_dwarf(dwarf, unit, namespace, iter)?)
            }
            gimli::DW_TAG_subroutine_type => {
                TypeKind::Subroutine(SubroutineType::parse_dwarf(dwarf, unit, namespace, iter)?)
            }
            gimli::DW_TAG_const_type => {
                TypeKind::Modifier(TypeModifier::parse_dwarf(dwarf,
                                                             unit,
                                                             namespace,
                                                             iter,
                                                             TypeModifierKind::Const)?)
            }
            gimli::DW_TAG_pointer_type => {
                TypeKind::Modifier(TypeModifier::parse_dwarf(dwarf,
                                                             unit,
                                                             namespace,
                                                             iter,
                                                             TypeModifierKind::Pointer)?)
            }
            gimli::DW_TAG_restrict_type => {
                TypeKind::Modifier(TypeModifier::parse_dwarf(dwarf,
                                                             unit,
                                                             namespace,
                                                             iter,
                                                             TypeModifierKind::Restrict)?)
            }
            _ => TypeKind::Unimplemented(tag),
        };
        Ok(ty)
    }

    fn from_offset<'a>(
        state: &PrintState<'a, 'input>,
        offset: gimli::UnitOffset
    ) -> Option<&'a Type<'input>> {
        state.types.get(&offset.0).map(|v| *v)
    }

    fn bit_size(&self, state: &PrintState) -> Option<u64> {
        match self.kind {
            TypeKind::Base(ref val) => val.bit_size(state),
            TypeKind::Def(ref val) => val.bit_size(state),
            TypeKind::Struct(ref val) => val.bit_size(state),
            TypeKind::Union(ref val) => val.bit_size(state),
            TypeKind::Enumeration(ref val) => val.bit_size(state),
            TypeKind::Array(ref val) => val.bit_size(state),
            TypeKind::Subroutine(ref val) => val.bit_size(state),
            TypeKind::Modifier(ref val) => val.bit_size(state),
            TypeKind::Unimplemented(_) => None,
        }
    }

    fn visit_members(&self, f: &mut FnMut(&Member) -> ()) {
        match self.kind {
            TypeKind::Struct(ref val) => val.visit_members(f),
            TypeKind::Union(ref val) => val.visit_members(f),
            TypeKind::Enumeration(..) |
            TypeKind::Def(..) |
            TypeKind::Base(..) |
            TypeKind::Array(..) |
            TypeKind::Subroutine(..) |
            TypeKind::Modifier(..) |
            TypeKind::Unimplemented(_) => {}
        }
    }

    fn print(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        match self.kind {
            TypeKind::Def(ref val) => val.print(w, state)?,
            TypeKind::Struct(ref val) => val.print(w, state)?,
            TypeKind::Union(ref val) => val.print(w, state)?,
            TypeKind::Enumeration(ref val) => val.print(w, state)?,
            TypeKind::Base(..) |
            TypeKind::Array(..) |
            TypeKind::Subroutine(..) |
            TypeKind::Modifier(..) => {}
            TypeKind::Unimplemented(_) => {
                state.line_start(w)?;
                self.print_name(w, state)?;
                writeln!(w, "")?;
            }
        }
        Ok(())
    }

    fn print_name(&self, w: &mut Write, state: &PrintState) -> Result<()> {
        match self.kind {
            TypeKind::Base(ref val) => val.print_name(w)?,
            TypeKind::Def(ref val) => val.print_name(w)?,
            TypeKind::Struct(ref val) => val.print_name(w)?,
            TypeKind::Union(ref val) => val.print_name(w)?,
            TypeKind::Enumeration(ref val) => val.print_name(w)?,
            TypeKind::Array(ref val) => val.print_name(w, state)?,
            TypeKind::Subroutine(ref val) => val.print_name(w, state)?,
            TypeKind::Modifier(ref val) => val.print_name(w, state)?,
            TypeKind::Unimplemented(ref tag) => write!(w, "<unimplemented {}>", tag)?,
        }
        Ok(())
    }

    fn print_offset_name(
        w: &mut Write,
        state: &PrintState,
        offset: gimli::UnitOffset
    ) -> Result<()> {
        match Type::from_offset(state, offset) {
            Some(ty) => ty.print_name(w, state)?,
            None => write!(w, "<invalid-type>")?,
        }
        Ok(())
    }

    fn is_anon(&self) -> bool {
        match self.kind {
            TypeKind::Struct(ref val) => val.is_anon(),
            TypeKind::Union(ref val) => val.is_anon(),
            TypeKind::Base(..) |
            TypeKind::Def(..) |
            TypeKind::Enumeration(..) |
            TypeKind::Array(..) |
            TypeKind::Subroutine(..) |
            TypeKind::Modifier(..) |
            TypeKind::Unimplemented(..) => false,
        }
    }

    fn cmp(type_a: &Type, type_b: &Type) -> cmp::Ordering {
        use TypeKind::*;
        match (&type_a.kind, &type_b.kind) {
            (&Def(ref a), &Def(ref b)) => TypeDef::cmp(a, b),
            (&Struct(ref a), &Struct(ref b)) => StructType::cmp(a, b),
            (&Union(ref a), &Union(ref b)) => UnionType::cmp(a, b),
            (&Enumeration(ref a), &Enumeration(ref b)) => EnumerationType::cmp(a, b),
            _ => type_a.kind.discriminant_value().cmp(&type_b.kind.discriminant_value()),
        }
    }

    fn equal(type_a: &Type, type_b: &Type, state: &DiffState) -> bool {
        use TypeKind::*;
        match (&type_a.kind, &type_b.kind) {
            (&Base(ref a), &Base(ref b)) => BaseType::equal(a, b, state),
            (&Def(ref a), &Def(ref b)) => TypeDef::equal(a, b, state),
            (&Struct(ref a), &Struct(ref b)) => StructType::equal(a, b, state),
            (&Union(ref a), &Union(ref b)) => UnionType::equal(a, b, state),
            (&Enumeration(ref a), &Enumeration(ref b)) => EnumerationType::equal(a, b, state),
            (&Array(ref a), &Array(ref b)) => ArrayType::equal(a, b, state),
            (&Subroutine(ref a), &Subroutine(ref b)) => SubroutineType::equal(a, b, state),
            (&Modifier(ref a), &Modifier(ref b)) => TypeModifier::equal(a, b, state),
            _ => false,
        }
    }

    fn diff(w: &mut Write, type_a: &Type, type_b: &Type, state: &mut DiffState) -> Result<()> {
        use TypeKind::*;
        match (&type_a.kind, &type_b.kind) {
            (&Def(ref a), &Def(ref b)) => {
                TypeDef::diff(w, a, b, state)?;
            }
            (&Struct(ref a), &Struct(ref b)) => {
                StructType::diff(w, a, b, state)?;
            }
            (&Union(ref a), &Union(ref b)) => {
                UnionType::diff(w, a, b, state)?;
            }
            (&Enumeration(ref a), &Enumeration(ref b)) => {
                EnumerationType::diff(w, a, b, state)?;
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug)]
struct TypeModifier<'input> {
    kind: TypeModifierKind,
    ty: Option<gimli::UnitOffset>,
    namespace: Rc<Namespace<'input>>,
    name: Option<&'input ffi::CStr>,
    byte_size: Option<u64>,
}

#[derive(Debug)]
enum TypeModifierKind {
    Const,
    Pointer,
    Restrict,
}

impl<'input> TypeModifier<'input> {
    fn parse_dwarf<'state, 'abbrev, 'unit, 'tree, Endian>(
        dwarf: &DwarfFileState<'input, Endian>,
        _unit: &mut DwarfUnitState<'state, 'input, Endian>,
        namespace: &Rc<Namespace<'input>>,
        mut iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>,
        kind: TypeModifierKind
    ) -> Result<TypeModifier<'input>>
        where Endian: gimli::Endianity
    {
        let mut modifier = TypeModifier {
            kind: kind,
            ty: None,
            namespace: namespace.clone(),
            name: None,
            byte_size: None,
        };

        {
            let mut attrs = iter.entry().unwrap().attrs();
            while let Some(attr) = attrs.next()? {
                match attr.name() {
                    gimli::DW_AT_name => {
                        modifier.name = attr.string_value(&dwarf.debug_str);
                    }
                    gimli::DW_AT_type => {
                        if let gimli::AttributeValue::UnitRef(offset) = attr.value() {
                            modifier.ty = Some(offset);
                        }
                    }
                    gimli::DW_AT_byte_size => {
                        modifier.byte_size = attr.udata_value();
                    }
                    _ => {
                        debug!("unknown type modifier attribute: {} {:?}",
                               attr.name(),
                               attr.value())
                    }
                }
            }
        }

        while let Some(child) = iter.next()? {
            match child.entry().unwrap().tag() {
                tag => {
                    debug!("unknown type modifier child tag: {}", tag);
                }
            }
        }
        Ok(modifier)
    }

    fn bit_size(&self, state: &PrintState) -> Option<u64> {
        if let Some(byte_size) = self.byte_size {
            return Some(byte_size * 8);
        }
        match self.kind {
            TypeModifierKind::Const |
            TypeModifierKind::Restrict => {
                self.ty
                    .and_then(|v| Type::from_offset(state, v))
                    .and_then(|v| v.bit_size(state))
            }
            TypeModifierKind::Pointer => state.address_size.map(|v| v * 8),
        }
    }

    fn print_name(&self, w: &mut Write, state: &PrintState) -> Result<()> {
        if let Some(name) = self.name {
            // Not sure namespace is required here.
            self.namespace.print(w)?;
            write!(w, "{}", name.to_string_lossy())?;
        } else {
            match self.kind {
                TypeModifierKind::Const => write!(w, "const ")?,
                TypeModifierKind::Pointer => write!(w, "* ")?,
                TypeModifierKind::Restrict => write!(w, "restrict ")?,
            }
            match self.ty {
                Some(ty) => Type::print_offset_name(w, state, ty)?,
                None => write!(w, "<unknown-type>")?,
            }
        }
        Ok(())
    }

    fn equal(_a: &TypeModifier, _b: &TypeModifier, _state: &DiffState) -> bool {
        // TODO
        false
    }
}

#[derive(Debug, Default)]
struct BaseType<'input> {
    name: Option<&'input ffi::CStr>,
    byte_size: Option<u64>,
}

impl<'input> BaseType<'input> {
    fn parse_dwarf<'state, 'abbrev, 'unit, 'tree, Endian>(
        dwarf: &DwarfFileState<'input, Endian>,
        _unit: &mut DwarfUnitState<'state, 'input, Endian>,
        _namespace: &Rc<Namespace<'input>>,
        mut iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
    ) -> Result<BaseType<'input>>
        where Endian: gimli::Endianity
    {
        let mut ty = BaseType::default();

        {
            let mut attrs = iter.entry().unwrap().attrs();
            while let Some(attr) = attrs.next()? {
                match attr.name() {
                    gimli::DW_AT_name => {
                        ty.name = attr.string_value(&dwarf.debug_str);
                    }
                    gimli::DW_AT_byte_size => {
                        ty.byte_size = attr.udata_value();
                    }
                    gimli::DW_AT_encoding => {}
                    _ => {
                        debug!("unknown base type attribute: {} {:?}",
                               attr.name(),
                               attr.value())
                    }
                }
            }
        }

        while let Some(child) = iter.next()? {
            match child.entry().unwrap().tag() {
                tag => {
                    debug!("unknown base type child tag: {}", tag);
                }
            }
        }
        Ok(ty)
    }

    fn bit_size(&self, _state: &PrintState) -> Option<u64> {
        self.byte_size.map(|v| v * 8)
    }

    fn print_name(&self, w: &mut Write) -> Result<()> {
        match self.name {
            Some(name) => write!(w, "{}", name.to_string_lossy())?,
            None => write!(w, "<anon-base-type>")?,
        }
        Ok(())
    }

    // Compare names
    fn cmp(a: &BaseType, b: &BaseType) -> cmp::Ordering {
        a.name.cmp(&b.name)
    }

    fn equal(a: &BaseType, b: &BaseType, _state: &DiffState) -> bool {
        if Self::cmp(a, b) != cmp::Ordering::Equal {
            return false;
        }
        a.byte_size == b.byte_size
    }
}

#[derive(Debug, Default)]
struct TypeDef<'input> {
    namespace: Rc<Namespace<'input>>,
    name: Option<&'input ffi::CStr>,
    ty: Option<gimli::UnitOffset>,
}

impl<'input> TypeDef<'input> {
    fn parse_dwarf<'state, 'abbrev, 'unit, 'tree, Endian>(
        dwarf: &DwarfFileState<'input, Endian>,
        _unit: &mut DwarfUnitState<'state, 'input, Endian>,
        namespace: &Rc<Namespace<'input>>,
        mut iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
    ) -> Result<TypeDef<'input>>
        where Endian: gimli::Endianity
    {
        let mut typedef = TypeDef::default();
        typedef.namespace = namespace.clone();

        {
            let mut attrs = iter.entry().unwrap().attrs();
            while let Some(attr) = attrs.next()? {
                match attr.name() {
                    gimli::DW_AT_name => {
                        typedef.name = attr.string_value(&dwarf.debug_str);
                    }
                    gimli::DW_AT_type => {
                        if let gimli::AttributeValue::UnitRef(offset) = attr.value() {
                            typedef.ty = Some(offset);
                        }
                    }
                    gimli::DW_AT_decl_file |
                    gimli::DW_AT_decl_line => {}
                    _ => {
                        debug!("unknown typedef attribute: {} {:?}",
                               attr.name(),
                               attr.value())
                    }
                }
            }
        }

        while let Some(child) = iter.next()? {
            match child.entry().unwrap().tag() {
                tag => {
                    debug!("unknown typedef child tag: {}", tag);
                }
            }
        }
        Ok(typedef)
    }

    fn ty<'a>(&self, state: &PrintState<'a, 'input>) -> Option<&'a Type<'input>> {
        self.ty.and_then(|t| Type::from_offset(state, t))
    }

    fn bit_size(&self, state: &PrintState) -> Option<u64> {
        self.ty(state).and_then(|v| v.bit_size(state))
    }

    fn print(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        if !state.filter_name(self.name) || !state.filter_namespace(&*self.namespace) {
            return Ok(());
        }

        state.line_start(w)?;
        write!(w, "type ")?;
        self.print_name(w)?;
        write!(w, " = ")?;
        if let Some(ty) = self.ty(state) {
            if ty.is_anon() {
                writeln!(w, "")?;

                state.indent(|state| ty.print(w, state))?;
            } else {
                ty.print_name(w, state)?;
                writeln!(w, "")?;

                state.indent(|state| {
                        if let Some(bit_size) = self.bit_size(state) {
                            state.line_start(w)?;
                            writeln!(w, "size: {}", format_bit(bit_size))?;
                        }
                        writeln!(w, "")?;
                        Ok(())
                    })?;
            }
        } else {
            writeln!(w, "<unknown-type>")?;
        }
        Ok(())
    }

    fn print_name(&self, w: &mut Write) -> Result<()> {
        self.namespace.print(w)?;
        match self.name {
            Some(name) => write!(w, "{}", name.to_string_lossy())?,
            None => write!(w, "<anon-typedef>")?,
        }
        Ok(())
    }

    // Compare names
    fn cmp(a: &TypeDef, b: &TypeDef) -> cmp::Ordering {
        cmp_ns_and_name(&a.namespace, a.name, &b.namespace, b.name)
    }

    fn equal(a: &TypeDef, b: &TypeDef, state: &DiffState) -> bool {
        if Self::cmp(a, b) != cmp::Ordering::Equal {
            return false;
        }
        match (a.ty(&state.a), b.ty(&state.b)) {
            (Some(a), Some(b)) => Type::equal(a, b, state),
            (None, None) => true,
            _ => false,
        }
    }

    fn diff(w: &mut Write, a: &TypeDef, b: &TypeDef, state: &mut DiffState) -> Result<()> {
        if Self::equal(a, b, state) {
            return Ok(());
        }
        // TODO
        write!(w, "- ")?;
        a.print(w, &mut state.a)?;
        write!(w, "+ ")?;
        b.print(w, &mut state.b)?;
        Ok(())
    }
}

#[derive(Debug, Default)]
struct StructType<'input> {
    namespace: Rc<Namespace<'input>>,
    name: Option<&'input ffi::CStr>,
    byte_size: Option<u64>,
    declaration: bool,
    members: Vec<Member<'input>>,
    subprograms: Vec<Subprogram<'input>>,
}

impl<'input> StructType<'input> {
    fn parse_dwarf<'state, 'abbrev, 'unit, 'tree, Endian>(
        dwarf: &DwarfFileState<'input, Endian>,
        unit: &mut DwarfUnitState<'state, 'input, Endian>,
        namespace: &Rc<Namespace<'input>>,
        mut iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
    ) -> Result<StructType<'input>>
        where Endian: gimli::Endianity
    {
        let mut ty = StructType::default();
        ty.namespace = namespace.clone();

        {
            let mut attrs = iter.entry().unwrap().attrs();
            while let Some(attr) = attrs.next()? {
                match attr.name() {
                    gimli::DW_AT_name => {
                        ty.name = attr.string_value(&dwarf.debug_str);
                    }
                    gimli::DW_AT_byte_size => {
                        ty.byte_size = attr.udata_value();
                    }
                    gimli::DW_AT_declaration => {
                        if let gimli::AttributeValue::Flag(flag) = attr.value() {
                            ty.declaration = flag;
                        }
                    }
                    gimli::DW_AT_decl_file |
                    gimli::DW_AT_decl_line |
                    gimli::DW_AT_sibling => {}
                    _ => {
                        debug!("unknown struct attribute: {} {:?}",
                               attr.name(),
                               attr.value())
                    }
                }
            }
        }

        let namespace = Namespace::new(&ty.namespace, ty.name);
        while let Some(child) = iter.next()? {
            match child.entry().unwrap().tag() {
                gimli::DW_TAG_subprogram => {
                    ty.subprograms
                        .push(Subprogram::parse_dwarf(dwarf, unit, &namespace, child)?);
                }
                gimli::DW_TAG_member => {
                    ty.members.push(Member::parse_dwarf(dwarf, unit, &namespace, child)?);
                }
                tag => {
                    debug!("unknown struct child tag: {}", tag);
                }
            }
        }
        Ok(ty)
    }

    fn bit_size(&self, _state: &PrintState) -> Option<u64> {
        self.byte_size.map(|v| v * 8)
    }

    fn visit_members(&self, f: &mut FnMut(&Member) -> ()) {
        for member in &self.members {
            f(member);
        }
    }

    fn print(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        if !state.filter_name(self.name) || !state.filter_namespace(&*self.namespace) {
            return Ok(());
        }

        state.line_start(w)?;
        self.print_name(w)?;
        writeln!(w, "")?;

        state.indent(|state| {
                if let Some(size) = self.byte_size {
                    state.line_start(w)?;
                    writeln!(w, "size: {}", size)?;
                } else if !self.declaration {
                    debug!("struct with no size");
                }

                if self.declaration {
                    state.line_start(w)?;
                    writeln!(w, "declaration: yes")?;
                }

                if !self.members.is_empty() {
                    state.line_start(w)?;
                    writeln!(w, "members:")?;
                    state.indent(|state| self.print_members(w, state, Some(0)))?;
                }

                writeln!(w, "")?;
                Ok(())
            })?;

        for subprogram in &self.subprograms {
            subprogram.print(w, state)?;
        }
        Ok(())
    }

    fn print_members(
        &self,
        w: &mut Write,
        state: &mut PrintState,
        mut bit_offset: Option<u64>
    ) -> Result<()> {
        for member in &self.members {
            member.print(w, state, &mut bit_offset)?;
        }
        if let (Some(bit_offset), Some(size)) = (bit_offset, self.byte_size) {
            if bit_offset < size * 8 {
                state.line_start(w)?;
                writeln!(w,
                         "{}[{}]\t<padding>",
                         format_bit(bit_offset),
                         format_bit(size * 8 - bit_offset))?;
            }
        }
        Ok(())
    }

    fn print_name(&self, w: &mut Write) -> Result<()> {
        write!(w, "struct ")?;
        self.namespace.print(w)?;
        match self.name {
            Some(name) => write!(w, "{}", name.to_string_lossy())?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn is_anon(&self) -> bool {
        self.name.is_none()
    }

    fn cmp(a: &StructType, b: &StructType) -> cmp::Ordering {
        cmp_ns_and_name(&a.namespace, a.name, &b.namespace, b.name)
    }

    fn equal(a: &StructType, b: &StructType, state: &DiffState) -> bool {
        Self::cmp(a, b) == cmp::Ordering::Equal && a.byte_size == b.byte_size &&
        a.declaration == b.declaration && Self::equal_members(a, b, state) &&
        Self::equal_subprograms(a, b, state)
    }

    fn equal_members(struct_a: &StructType, struct_b: &StructType, state: &DiffState) -> bool {
        if struct_a.members.len() != struct_b.members.len() {
            return false;
        }
        for (a, b) in struct_a.members.iter().zip(struct_b.members.iter()) {
            if !Member::equal(a, b, state) {
                return false;
            }
        }
        true
    }

    fn equal_subprograms(
        _struct_a: &StructType,
        _struct_b: &StructType,
        _state: &DiffState
    ) -> bool {
        // TODO
        true
    }

    fn diff(w: &mut Write, a: &StructType, b: &StructType, state: &mut DiffState) -> Result<()> {
        if Self::equal(a, b, state) {
            return Ok(());
        }
        // TODO
        write!(w, "- ")?;
        a.print(w, &mut state.a)?;
        write!(w, "+ ")?;
        b.print(w, &mut state.b)?;
        Ok(())
    }
}

#[derive(Debug, Default)]
struct UnionType<'input> {
    namespace: Rc<Namespace<'input>>,
    name: Option<&'input ffi::CStr>,
    byte_size: Option<u64>,
    declaration: bool,
    members: Vec<Member<'input>>,
    subprograms: Vec<Subprogram<'input>>,
}

impl<'input> UnionType<'input> {
    fn parse_dwarf<'state, 'abbrev, 'unit, 'tree, Endian>(
        dwarf: &DwarfFileState<'input, Endian>,
        unit: &mut DwarfUnitState<'state, 'input, Endian>,
        namespace: &Rc<Namespace<'input>>,
        mut iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
    ) -> Result<UnionType<'input>>
        where Endian: gimli::Endianity
    {
        let mut ty = UnionType::default();
        ty.namespace = namespace.clone();

        {
            let mut attrs = iter.entry().unwrap().attrs();
            while let Some(attr) = attrs.next()? {
                match attr.name() {
                    gimli::DW_AT_name => {
                        ty.name = attr.string_value(&dwarf.debug_str);
                    }
                    gimli::DW_AT_byte_size => {
                        ty.byte_size = attr.udata_value();
                    }
                    gimli::DW_AT_declaration => {
                        if let gimli::AttributeValue::Flag(flag) = attr.value() {
                            ty.declaration = flag;
                        }
                    }
                    gimli::DW_AT_decl_file |
                    gimli::DW_AT_decl_line |
                    gimli::DW_AT_sibling => {}
                    _ => {
                        debug!("unknown union attribute: {} {:?}",
                               attr.name(),
                               attr.value())
                    }
                }
            }
        }

        let namespace = Namespace::new(&ty.namespace, ty.name);
        while let Some(child) = iter.next()? {
            match child.entry().unwrap().tag() {
                gimli::DW_TAG_subprogram => {
                    ty.subprograms
                        .push(Subprogram::parse_dwarf(dwarf, unit, &namespace, child)?);
                }
                gimli::DW_TAG_member => {
                    ty.members.push(Member::parse_dwarf(dwarf, unit, &namespace, child)?);
                }
                tag => {
                    debug!("unknown union child tag: {}", tag);
                }
            }
        }
        Ok(ty)
    }

    fn bit_size(&self, _state: &PrintState) -> Option<u64> {
        self.byte_size.map(|v| v * 8)
    }

    fn visit_members(&self, f: &mut FnMut(&Member) -> ()) {
        for member in &self.members {
            f(member);
        }
    }

    fn print(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        if !state.filter_name(self.name) || !state.filter_namespace(&*self.namespace) {
            return Ok(());
        }

        state.line_start(w)?;
        self.print_name(w)?;
        writeln!(w, "")?;

        state.indent(|state| {
                if let Some(size) = self.byte_size {
                    state.line_start(w)?;
                    writeln!(w, "size: {}", size)?;
                } else if !self.declaration {
                    debug!("union with no size");
                }

                if self.declaration {
                    state.line_start(w)?;
                    writeln!(w, "declaration: yes")?;
                }

                if !self.members.is_empty() {
                    state.line_start(w)?;
                    writeln!(w, "members:")?;
                    state.indent(|state| self.print_members(w, state, Some(0)))?;
                }

                writeln!(w, "")?;
                Ok(())
            })?;

        for subprogram in &self.subprograms {
            subprogram.print(w, state)?;
        }
        Ok(())
    }

    fn print_members(
        &self,
        w: &mut Write,
        state: &mut PrintState,
        bit_offset: Option<u64>
    ) -> Result<()> {
        for member in &self.members {
            let mut bit_offset = bit_offset;
            member.print(w, state, &mut bit_offset)?;
        }
        Ok(())
    }

    fn print_name(&self, w: &mut Write) -> Result<()> {
        write!(w, "union ")?;
        self.namespace.print(w)?;
        match self.name {
            Some(name) => write!(w, "{}", name.to_string_lossy())?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn is_anon(&self) -> bool {
        self.name.is_none()
    }

    fn cmp(a: &UnionType, b: &UnionType) -> cmp::Ordering {
        cmp_ns_and_name(&a.namespace, a.name, &b.namespace, b.name)
    }

    fn equal(a: &UnionType, b: &UnionType, state: &DiffState) -> bool {
        Self::cmp(a, b) == cmp::Ordering::Equal && a.byte_size == b.byte_size &&
        a.declaration == b.declaration && Self::equal_members(a, b, state) &&
        Self::equal_subprograms(a, b, state)
    }

    fn equal_members(union_a: &UnionType, union_b: &UnionType, state: &DiffState) -> bool {
        if union_a.members.len() != union_b.members.len() {
            return false;
        }
        for (a, b) in union_a.members.iter().zip(union_b.members.iter()) {
            if !Member::equal(a, b, state) {
                return false;
            }
        }
        true
    }

    fn equal_subprograms(_union_a: &UnionType, _union_b: &UnionType, _state: &DiffState) -> bool {
        // TODO
        true
    }

    fn diff(w: &mut Write, a: &UnionType, b: &UnionType, state: &mut DiffState) -> Result<()> {
        if Self::equal(a, b, state) {
            return Ok(());
        }
        // TODO
        write!(w, "- ")?;
        a.print(w, &mut state.a)?;
        write!(w, "+ ")?;
        b.print(w, &mut state.b)?;
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Member<'input> {
    name: Option<&'input ffi::CStr>,
    ty: Option<gimli::UnitOffset>,
    // Defaults to 0, so always present.
    bit_offset: u64,
    bit_size: Option<u64>,
}

impl<'input> Member<'input> {
    fn parse_dwarf<'state, 'abbrev, 'unit, 'tree, Endian>(
        dwarf: &DwarfFileState<'input, Endian>,
        unit: &mut DwarfUnitState<'state, 'input, Endian>,
        _namespace: &Rc<Namespace<'input>>,
        mut iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
    ) -> Result<Member<'input>>
        where Endian: gimli::Endianity
    {
        let mut member = Member::default();
        let mut bit_offset = None;
        let mut byte_size = None;

        {
            let mut attrs = iter.entry().unwrap().attrs();
            while let Some(attr) = attrs.next()? {
                match attr.name() {
                    gimli::DW_AT_name => {
                        member.name = attr.string_value(&dwarf.debug_str);
                    }
                    gimli::DW_AT_type => {
                        if let gimli::AttributeValue::UnitRef(offset) = attr.value() {
                            member.ty = Some(offset);
                        }
                    }
                    gimli::DW_AT_data_member_location => {
                        match attr.value() {
                            gimli::AttributeValue::Udata(v) => member.bit_offset = v * 8,
                            gimli::AttributeValue::Exprloc(expr) => {
                                match evaluate(unit.header, expr.0) {
                                    Ok(gimli::Location::Address { address }) => {
                                        member.bit_offset = address * 8;
                                    }
                                    Ok(loc) => {
                                        debug!("unknown DW_AT_data_member_location result: {:?}",
                                               loc)
                                    }
                                    Err(e) => {
                                        debug!("DW_AT_data_member_location evaluation failed: {}",
                                               e)
                                    }
                                }
                            }
                            _ => {
                                debug!("unknown DW_AT_data_member_location: {:?}", attr.value());
                            }
                        }
                    }
                    gimli::DW_AT_data_bit_offset => {
                        if let Some(bit_offset) = attr.udata_value() {
                            member.bit_offset = bit_offset;
                        }
                    }
                    gimli::DW_AT_bit_offset => {
                        bit_offset = attr.udata_value();
                    }
                    gimli::DW_AT_byte_size => {
                        byte_size = attr.udata_value();
                    }
                    gimli::DW_AT_bit_size => {
                        member.bit_size = attr.udata_value();
                    }
                    gimli::DW_AT_decl_file |
                    gimli::DW_AT_decl_line => {}
                    _ => {
                        debug!("unknown member attribute: {} {:?}",
                               attr.name(),
                               attr.value());
                    }
                }
            }
        }

        if let (Some(bit_offset), Some(bit_size)) = (bit_offset, member.bit_size) {
            // DWARF version 2/3, but allowed in later versions for compatibility.
            // The data member is a bit field contained in an anonymous object.
            // member.bit_offset starts as the offset of the anonymous object.
            // byte_size is the size of the anonymous object.
            // bit_offset is the offset from the anonymous object MSB to the bit field MSB.
            // bit_size is the size of the bit field.
            if Endian::is_big_endian() {
                // For big endian, the MSB is the first bit, so we simply add bit_offset,
                // and byte_size is unneeded.
                member.bit_offset = member.bit_offset.wrapping_add(bit_offset);
            } else {
                // For little endian, we have to work backwards, so we need byte_size.
                if let Some(byte_size) = byte_size {
                    // First find the offset of the MSB of the anonymous object.
                    member.bit_offset = member.bit_offset.wrapping_add(byte_size * 8);
                    // Then go backwards to the LSB of the bit field.
                    member.bit_offset = member.bit_offset
                        .wrapping_sub(bit_offset.wrapping_add(bit_size));
                } else {
                    // DWARF version 2/3 says the byte_size can be inferred,
                    // but it is unclear when this would be useful.
                    // Delay implementing this until needed.
                    debug!("missing byte_size for bit field offset");
                }
            }
        } else if byte_size.is_some() {
            // TODO: should this set member.bit_size?
            debug!("ignored member byte_size");
        }

        while let Some(child) = iter.next()? {
            match child.entry().unwrap().tag() {
                tag => {
                    debug!("unknown member child tag: {}", tag);
                }
            }
        }
        Ok(member)
    }

    fn bit_size(&self, state: &PrintState) -> Option<u64> {
        if self.bit_size.is_some() {
            self.bit_size
        } else if let Some(ty) = self.ty {
            Type::from_offset(state, ty).and_then(|v| v.bit_size(state))
        } else {
            None
        }
    }

    fn print(
        &self,
        w: &mut Write,
        state: &mut PrintState,
        end_bit_offset: &mut Option<u64>
    ) -> Result<()> {
        if let Some(end_bit_offset) = *end_bit_offset {
            if self.bit_offset > end_bit_offset {
                state.line_start(w)?;
                writeln!(w,
                         "{}[{}]\t<padding>",
                         format_bit(end_bit_offset),
                         format_bit(self.bit_offset - end_bit_offset))?;
            }
        }

        state.line_start(w)?;
        write!(w, "{}", format_bit(self.bit_offset))?;
        match self.bit_size(state) {
            Some(bit_size) => {
                write!(w, "[{}]", format_bit(bit_size))?;
                *end_bit_offset = self.bit_offset.checked_add(bit_size);
            }
            None => {
                // TODO: show element size for unsized arrays.
                debug!("no size for {:?}", self);
                write!(w, "[??]")?;
                *end_bit_offset = None;
            }
        }
        match self.name {
            Some(name) => write!(w, "\t{}", name.to_string_lossy())?,
            None => write!(w, "\t<anon>")?,
        }
        if let Some(ty) = self.ty.and_then(|v| Type::from_offset(state, v)) {
            write!(w, ": ")?;
            ty.print_name(w, state)?;
            writeln!(w, "")?;
            if self.is_inline(state) {
                state.indent(|state| {
                        match ty.kind {
                            TypeKind::Struct(ref t) => {
                                t.print_members(w, state, Some(self.bit_offset))?;
                            }
                            TypeKind::Union(ref t) => {
                                t.print_members(w, state, Some(self.bit_offset))?;
                            }
                            _ => {
                                debug!("unknown anon member: {:?}", ty);
                            }
                        }
                        Ok(())
                    })?;
            }
        } else {
            writeln!(w, ": <invalid-type>")?;
        }
        Ok(())
    }

    fn is_inline(&self, state: &PrintState) -> bool {
        match self.name {
            Some(s) => {
                if s.to_bytes().starts_with(b"RUST$ENCODED$ENUM$") {
                    return true;
                }
            }
            None => return true,
        };
        if let Some(ty) = self.ty.and_then(|v| Type::from_offset(state, v)) {
            ty.is_anon()
        } else {
            false
        }
    }

    fn equal(_a: &Member, _b: &Member, _state: &DiffState) -> bool {
        // TODO
        true
    }
}

#[derive(Debug, Default)]
struct EnumerationType<'input> {
    namespace: Rc<Namespace<'input>>,
    name: Option<&'input ffi::CStr>,
    byte_size: Option<u64>,
    enumerators: Vec<Enumerator<'input>>,
    subprograms: Vec<Subprogram<'input>>,
}

impl<'input> EnumerationType<'input> {
    fn parse_dwarf<'state, 'abbrev, 'unit, 'tree, Endian>(
        dwarf: &DwarfFileState<'input, Endian>,
        unit: &mut DwarfUnitState<'state, 'input, Endian>,
        namespace: &Rc<Namespace<'input>>,
        mut iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
    ) -> Result<EnumerationType<'input>>
        where Endian: gimli::Endianity
    {
        let mut ty = EnumerationType::default();
        ty.namespace = namespace.clone();

        {
            let mut attrs = iter.entry().unwrap().attrs();
            while let Some(attr) = attrs.next()? {
                match attr.name() {
                    gimli::DW_AT_name => {
                        ty.name = attr.string_value(&dwarf.debug_str);
                    }
                    gimli::DW_AT_byte_size => {
                        ty.byte_size = attr.udata_value();
                    }
                    gimli::DW_AT_decl_file |
                    gimli::DW_AT_decl_line |
                    gimli::DW_AT_sibling |
                    gimli::DW_AT_type |
                    gimli::DW_AT_enum_class => {}
                    _ => {
                        debug!("unknown enumeration attribute: {} {:?}",
                               attr.name(),
                               attr.value())
                    }
                }
            }
        }

        let namespace = Namespace::new(&ty.namespace, ty.name);
        while let Some(child) = iter.next()? {
            match child.entry().unwrap().tag() {
                gimli::DW_TAG_enumerator => {
                    ty.enumerators
                        .push(Enumerator::parse_dwarf(dwarf, unit, &namespace, child)?);
                }
                gimli::DW_TAG_subprogram => {
                    ty.subprograms
                        .push(Subprogram::parse_dwarf(dwarf, unit, &namespace, child)?);
                }
                tag => {
                    debug!("unknown enumeration child tag: {}", tag);
                }
            }
        }
        Ok(ty)
    }

    fn bit_size(&self, _state: &PrintState) -> Option<u64> {
        self.byte_size.map(|v| v * 8)
    }

    fn print(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        if !state.filter_name(self.name) || !state.filter_namespace(&*self.namespace) {
            return Ok(());
        }

        state.line_start(w)?;
        self.print_name(w)?;
        writeln!(w, "")?;

        state.indent(|state| {
                if let Some(size) = self.byte_size {
                    state.line_start(w)?;
                    writeln!(w, "size: {}", size)?;
                }

                if !self.enumerators.is_empty() {
                    state.line_start(w)?;
                    writeln!(w, "enumerators:")?;
                    state.indent(|state| {
                            for enumerator in &self.enumerators {
                                enumerator.print(w, state)?;
                            }
                            Ok(())
                        })?;
                }

                writeln!(w, "")?;
                Ok(())
            })?;

        for subprogram in &self.subprograms {
            subprogram.print(w, state)?;
        }
        Ok(())
    }

    fn print_name(&self, w: &mut Write) -> Result<()> {
        write!(w, "enum ")?;
        self.namespace.print(w)?;
        match self.name {
            Some(name) => write!(w, "{}", name.to_string_lossy())?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn cmp(a: &EnumerationType, b: &EnumerationType) -> cmp::Ordering {
        cmp_ns_and_name(&a.namespace, a.name, &b.namespace, b.name)
    }

    fn equal(a: &EnumerationType, b: &EnumerationType, state: &DiffState) -> bool {
        Self::cmp(a, b) == cmp::Ordering::Equal && a.byte_size == b.byte_size &&
        Self::equal_enumerators(a, b, state) && Self::equal_subprograms(a, b, state)
    }

    fn equal_enumerators(
        enum_a: &EnumerationType,
        enum_b: &EnumerationType,
        state: &DiffState
    ) -> bool {
        if enum_a.enumerators.len() != enum_b.enumerators.len() {
            return false;
        }
        for (a, b) in enum_a.enumerators.iter().zip(enum_b.enumerators.iter()) {
            if !Enumerator::equal(a, b, state) {
                return false;
            }
        }
        true
    }

    fn equal_subprograms(
        _enum_a: &EnumerationType,
        _enum_b: &EnumerationType,
        _state: &DiffState
    ) -> bool {
        // TODO
        true
    }

    fn diff(
        w: &mut Write,
        a: &EnumerationType,
        b: &EnumerationType,
        state: &mut DiffState
    ) -> Result<()> {
        if Self::equal(a, b, state) {
            return Ok(());
        }
        // TODO
        write!(w, "- ")?;
        a.print(w, &mut state.a)?;
        write!(w, "+ ")?;
        b.print(w, &mut state.b)?;
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Enumerator<'input> {
    name: Option<&'input ffi::CStr>,
    value: Option<i64>,
}

impl<'input> Enumerator<'input> {
    fn parse_dwarf<'state, 'abbrev, 'unit, 'tree, Endian>(
        dwarf: &DwarfFileState<'input, Endian>,
        _unit: &mut DwarfUnitState<'state, 'input, Endian>,
        _namespace: &Rc<Namespace<'input>>,
        mut iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
    ) -> Result<Enumerator<'input>>
        where Endian: gimli::Endianity
    {
        let mut enumerator = Enumerator::default();

        {
            let mut attrs = iter.entry().unwrap().attrs();
            while let Some(ref attr) = attrs.next()? {
                match attr.name() {
                    gimli::DW_AT_name => {
                        enumerator.name = attr.string_value(&dwarf.debug_str);
                    }
                    gimli::DW_AT_const_value => {
                        if let Some(value) = attr.sdata_value() {
                            enumerator.value = Some(value);
                        } else {
                            debug!("unknown enumerator const_value: {:?}", attr.value());
                        }
                    }
                    _ => {
                        debug!("unknown enumerator attribute: {} {:?}",
                               attr.name(),
                               attr.value())
                    }
                }
            }
        }

        while let Some(child) = iter.next()? {
            match child.entry().unwrap().tag() {
                tag => {
                    debug!("unknown enumerator child tag: {}", tag);
                }
            }
        }
        Ok(enumerator)
    }

    fn print(&self, w: &mut Write, state: &PrintState) -> Result<()> {
        state.line_start(w)?;
        self.print_name(w)?;
        if let Some(value) = self.value {
            write!(w, "({})", value)?;
        }
        writeln!(w, "")?;
        Ok(())
    }

    fn print_name(&self, w: &mut Write) -> Result<()> {
        match self.name {
            Some(name) => write!(w, "{}", name.to_string_lossy())?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn equal(_a: &Enumerator, _b: &Enumerator, _state: &DiffState) -> bool {
        // TODO
        true
    }
}

#[derive(Debug, Default)]
struct ArrayType<'input> {
    ty: Option<gimli::UnitOffset>,
    count: Option<u64>,
    phantom: std::marker::PhantomData<&'input [u8]>,
}

impl<'input> ArrayType<'input> {
    fn parse_dwarf<'state, 'abbrev, 'unit, 'tree, Endian>(
        _dwarf: &DwarfFileState<'input, Endian>,
        _unit: &mut DwarfUnitState<'state, 'input, Endian>,
        _namespace: &Rc<Namespace<'input>>,
        mut iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
    ) -> Result<ArrayType<'input>>
        where Endian: gimli::Endianity
    {
        let mut array = ArrayType::default();

        {
            let mut attrs = iter.entry().unwrap().attrs();
            while let Some(attr) = attrs.next()? {
                match attr.name() {
                    gimli::DW_AT_type => {
                        if let gimli::AttributeValue::UnitRef(offset) = attr.value() {
                            array.ty = Some(offset);
                        }
                    }
                    gimli::DW_AT_sibling => {}
                    _ => {
                        debug!("unknown array attribute: {} {:?}",
                               attr.name(),
                               attr.value())
                    }
                }
            }
        }

        while let Some(child) = iter.next()? {
            match child.entry().unwrap().tag() {
                gimli::DW_TAG_subrange_type => {
                    let mut attrs = child.entry().unwrap().attrs();
                    while let Some(attr) = attrs.next()? {
                        match attr.name() {
                            gimli::DW_AT_count => {
                                array.count = attr.udata_value();
                            }
                            gimli::DW_AT_upper_bound => {
                                // TODO: use AT_lower_bound too
                                array.count = attr.udata_value();
                            }
                            gimli::DW_AT_type |
                            gimli::DW_AT_lower_bound => {}
                            _ => {
                                debug!("unknown array subrange attribute: {} {:?}",
                                       attr.name(),
                                       attr.value())
                            }
                        }
                    }
                }
                tag => {
                    debug!("unknown array child tag: {}", tag);
                }
            }
        }
        Ok(array)
    }

    fn bit_size(&self, state: &PrintState) -> Option<u64> {
        if let (Some(ty), Some(count)) = (self.ty, self.count) {
            Type::from_offset(state, ty).and_then(|t| t.bit_size(state)).map(|v| v * count)
        } else {
            None
        }
    }

    fn print_name(&self, w: &mut Write, state: &PrintState) -> Result<()> {
        write!(w, "[")?;
        match self.ty {
            Some(ty) => Type::print_offset_name(w, state, ty)?,
            None => write!(w, "<unknown-type>")?,
        }
        if let Some(count) = self.count {
            write!(w, "; {}", count)?;
        }
        write!(w, "]")?;
        Ok(())
    }

    fn equal(_a: &ArrayType, _b: &ArrayType, _state: &DiffState) -> bool {
        // TODO
        false
    }
}

#[derive(Debug, Default)]
struct SubroutineType<'input> {
    parameters: Vec<Parameter<'input>>,
    return_type: Option<gimli::UnitOffset>,
}

impl<'input> SubroutineType<'input> {
    fn parse_dwarf<'state, 'abbrev, 'unit, 'tree, Endian>(
        dwarf: &DwarfFileState<'input, Endian>,
        unit: &mut DwarfUnitState<'state, 'input, Endian>,
        namespace: &Rc<Namespace<'input>>,
        mut iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
    ) -> Result<SubroutineType<'input>>
        where Endian: gimli::Endianity
    {
        let mut subroutine = SubroutineType::default();

        {
            let mut attrs = iter.entry().unwrap().attrs();
            while let Some(attr) = attrs.next()? {
                match attr.name() {
                    gimli::DW_AT_type => {
                        if let gimli::AttributeValue::UnitRef(offset) = attr.value() {
                            subroutine.return_type = Some(offset);
                        }
                    }
                    gimli::DW_AT_prototyped |
                    gimli::DW_AT_sibling => {}
                    _ => {
                        debug!("unknown subroutine attribute: {} {:?}",
                               attr.name(),
                               attr.value())
                    }
                }
            }
        }

        while let Some(child) = iter.next()? {
            match child.entry().unwrap().tag() {
                gimli::DW_TAG_formal_parameter => {
                    subroutine.parameters
                        .push(Parameter::parse_dwarf(dwarf, unit, namespace, child)?);
                }
                tag => {
                    debug!("unknown subroutine child tag: {}", tag);
                }
            }
        }
        Ok(subroutine)
    }

    fn bit_size(&self, _state: &PrintState) -> Option<u64> {
        None
    }

    fn print_name(&self, w: &mut Write, state: &PrintState) -> Result<()> {
        let mut first = true;
        write!(w, "(")?;
        for parameter in &self.parameters {
            if first {
                first = false;
            } else {
                write!(w, ", ")?;
            }
            parameter.print(w, state)?;
        }
        write!(w, ")")?;

        if let Some(return_type) = self.return_type {
            write!(w, " -> ")?;
            Type::print_offset_name(w, state, return_type)?;
        }
        Ok(())
    }

    fn equal(_a: &SubroutineType, _b: &SubroutineType, _state: &DiffState) -> bool {
        // TODO
        false
    }
}

#[derive(Debug)]
struct Subprogram<'input> {
    offset: gimli::UnitOffset,
    namespace: Rc<Namespace<'input>>,
    name: Option<&'input ffi::CStr>,
    linkage_name: Option<&'input ffi::CStr>,
    low_pc: Option<u64>,
    high_pc: Option<u64>,
    size: Option<u64>,
    inline: bool,
    declaration: bool,
    parameters: Vec<Parameter<'input>>,
    return_type: Option<gimli::UnitOffset>,
    inlined_subroutines: Vec<InlinedSubroutine<'input>>,
    variables: Vec<Variable<'input>>,
}

impl<'input> Subprogram<'input> {
    fn parse_dwarf<'state, 'abbrev, 'unit, 'tree, Endian>(
        dwarf: &DwarfFileState<'input, Endian>,
        unit: &mut DwarfUnitState<'state, 'input, Endian>,
        namespace: &Rc<Namespace<'input>>,
        mut iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
    ) -> Result<Subprogram<'input>>
        where Endian: gimli::Endianity
    {
        let mut subprogram = Subprogram {
            offset: iter.entry().unwrap().offset(),
            namespace: namespace.clone(),
            name: None,
            linkage_name: None,
            low_pc: None,
            high_pc: None,
            size: None,
            inline: false,
            declaration: false,
            parameters: Vec::new(),
            return_type: None,
            inlined_subroutines: Vec::new(),
            variables: Vec::new(),
        };

        {
            let entry = iter.entry().unwrap();
            let mut attrs = entry.attrs();
            while let Some(attr) = attrs.next()? {
                match attr.name() {
                    gimli::DW_AT_name => {
                        subprogram.name = attr.string_value(&dwarf.debug_str);
                    }
                    gimli::DW_AT_linkage_name => {
                        subprogram.linkage_name = attr.string_value(&dwarf.debug_str);
                    }
                    gimli::DW_AT_inline => {
                        if let gimli::AttributeValue::Inline(val) = attr.value() {
                            match val {
                                gimli::DW_INL_inlined |
                                gimli::DW_INL_declared_inlined => subprogram.inline = true,
                                _ => subprogram.inline = false,
                            }
                        }
                    }
                    gimli::DW_AT_low_pc => {
                        if let gimli::AttributeValue::Addr(addr) = attr.value() {
                            subprogram.low_pc = Some(addr);
                        }
                    }
                    gimli::DW_AT_high_pc => {
                        match attr.value() {
                            gimli::AttributeValue::Addr(addr) => subprogram.high_pc = Some(addr),
                            gimli::AttributeValue::Udata(size) => subprogram.size = Some(size),
                            _ => {}
                        }
                    }
                    gimli::DW_AT_type => {
                        if let gimli::AttributeValue::UnitRef(offset) = attr.value() {
                            subprogram.return_type = Some(offset);
                        }
                    }
                    gimli::DW_AT_declaration => {
                        if let gimli::AttributeValue::Flag(flag) = attr.value() {
                            subprogram.declaration = flag;
                        }
                    }
                    gimli::DW_AT_decl_file |
                    gimli::DW_AT_decl_line |
                    gimli::DW_AT_frame_base |
                    gimli::DW_AT_external |
                    gimli::DW_AT_abstract_origin |
                    gimli::DW_AT_GNU_all_call_sites |
                    gimli::DW_AT_GNU_all_tail_call_sites |
                    gimli::DW_AT_prototyped |
                    gimli::DW_AT_sibling => {}
                    _ => {
                        debug!("unknown subprogram attribute: {} {:?}",
                               attr.name(),
                               attr.value())
                    }
                }
            }

            if let Some(low_pc) = subprogram.low_pc {
                if let Some(high_pc) = subprogram.high_pc {
                    subprogram.size = high_pc.checked_sub(low_pc);
                } else if let Some(size) = subprogram.size {
                    subprogram.high_pc = low_pc.checked_add(size);
                }
            }
        }

        while let Some(child) = iter.next()? {
            match child.entry().unwrap().tag() {
                gimli::DW_TAG_formal_parameter => {
                    subprogram.parameters
                        .push(Parameter::parse_dwarf(dwarf, unit, namespace, child)?);
                }
                gimli::DW_TAG_inlined_subroutine => {
                    subprogram.inlined_subroutines
                        .push(InlinedSubroutine::parse_dwarf(dwarf, unit, namespace, child)?);
                }
                gimli::DW_TAG_variable => {
                    subprogram.variables
                        .push(Variable::parse_dwarf(dwarf, unit, namespace, child)?);
                }
                gimli::DW_TAG_lexical_block => {
                    parse_dwarf_lexical_block(&mut subprogram.inlined_subroutines,
                                              &mut subprogram.variables,
                                              dwarf,
                                              unit,
                                              namespace,
                                              child)?;
                }
                gimli::DW_TAG_template_type_parameter |
                gimli::DW_TAG_label |
                gimli::DW_TAG_structure_type |
                gimli::DW_TAG_union_type |
                gimli::DW_TAG_GNU_call_site => {}
                tag => {
                    debug!("unknown subprogram child tag: {}", tag);
                }
            }
        }

        Ok(subprogram)
    }

    fn from_offset<'a>(
        state: &PrintState<'a, 'input>,
        offset: gimli::UnitOffset
    ) -> Option<&'a Subprogram<'input>> {
        state.subprograms.get(&offset.0).map(|v| *v)
    }

    fn print(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        if !state.filter_name(self.name) || !state.filter_namespace(&*self.namespace) {
            return Ok(());
        }

        state.line_start(w)?;
        write!(w, "fn ")?;
        self.namespace.print(w)?;
        match self.name {
            Some(name) => write!(w, "{}", name.to_string_lossy())?,
            None => write!(w, "<anon>")?,
        }
        writeln!(w, "")?;

        state.indent(|state| {
            if let Some(linkage_name) = self.linkage_name {
                state.line_start(w)?;
                writeln!(w, "linkage name: {}", linkage_name.to_string_lossy())?;
            }

            if let (Some(low_pc), Some(high_pc)) = (self.low_pc, self.high_pc) {
                if high_pc > low_pc {
                    state.line_start(w)?;
                    writeln!(w, "address: 0x{:x}-0x{:x}", low_pc, high_pc - 1)?;
                } else {
                    state.line_start(w)?;
                    writeln!(w, "address: 0x{:x}", low_pc)?;
                }
            } else if !self.inline && !self.declaration {
                debug!("non-inline subprogram with no address");
            }

            if let Some(size) = self.size {
                state.line_start(w)?;
                writeln!(w, "size: {}", size)?;
            }

            if self.inline {
                state.line_start(w)?;
                writeln!(w, "inline: yes")?;
            }
            if self.declaration {
                state.line_start(w)?;
                writeln!(w, "declaration: yes")?;
            }

            if let Some(return_type) = self.return_type {
                state.line_start(w)?;
                writeln!(w, "return type:")?;
                state.indent(|state| {
                        state.line_start(w)?;
                        match Type::from_offset(state, return_type)
                            .and_then(|t| t.bit_size(state)) {
                            Some(bit_size) => write!(w, "[{}]", format_bit(bit_size))?,
                            None => write!(w, "[??]")?,
                        }
                        write!(w, "\t")?;
                        Type::print_offset_name(w, state, return_type)?;
                        writeln!(w, "")?;
                        Ok(())
                    })?;
            }

            if !self.parameters.is_empty() {
                state.line_start(w)?;
                writeln!(w, "parameters:")?;
                state.indent(|state| {
                        for parameter in &self.parameters {
                            state.line_start(w)?;
                            match parameter.bit_size(state) {
                                Some(bit_size) => write!(w, "[{}]", format_bit(bit_size))?,
                                None => write!(w, "[??]")?,
                            }
                            write!(w, "\t")?;
                            parameter.print(w, state)?;
                            writeln!(w, "")?;
                        }
                        Ok(())
                    })?;
            }

            if state.flags.inline_depth > 0 && !self.inlined_subroutines.is_empty() {
                state.line_start(w)?;
                writeln!(w, "inlined subroutines:")?;
                state.indent(|state| {
                        for subroutine in &self.inlined_subroutines {
                            subroutine.print(w, state, 1)?;
                        }
                        Ok(())
                    })?;
            }

            if let (Some(low_pc), Some(high_pc)) = (self.low_pc, self.high_pc) {
                if low_pc != 0 && state.flags.calls {
                    let calls =
                        disassemble(state.file.machine, &state.file.region, low_pc, high_pc);
                    if !calls.is_empty() {
                        state.line_start(w)?;
                        writeln!(w, "calls:")?;
                        state.indent(|state| {
                                for call in &calls {
                                    state.line_start(w)?;
                                    write!(w, "0x{:x}", call)?;
                                    if let Some(subprogram) = state.all_subprograms.get(call) {
                                        write!(w, " ")?;
                                        subprogram.print_name(w)?;
                                    }
                                    writeln!(w, "")?;
                                }
                                Ok(())
                            })?;
                    }
                }
            }

            writeln!(w, "")?;
            Ok(())
        })
    }

    fn print_name(&self, w: &mut Write) -> Result<()> {
        self.namespace.print(w)?;
        match self.name {
            Some(name) => write!(w, "{}", name.to_string_lossy())?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn cmp(a: &Subprogram, b: &Subprogram) -> cmp::Ordering {
        cmp_ns_and_name(&a.namespace, a.name, &b.namespace, b.name)
    }

    fn equal(a: &Subprogram, b: &Subprogram, _state: &DiffState) -> bool {
        if Self::cmp(a, b) != cmp::Ordering::Equal {
            return false;
        }
        // TODO
        true
    }

    fn diff(w: &mut Write, a: &Subprogram, b: &Subprogram, state: &mut DiffState) -> Result<()> {
        if Self::equal(a, b, state) {
            return Ok(());
        }
        // TODO
        write!(w, "- ")?;
        a.print(w, &mut state.a)?;
        write!(w, "+ ")?;
        b.print(w, &mut state.b)?;
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Parameter<'input> {
    name: Option<&'input ffi::CStr>,
    ty: Option<gimli::UnitOffset>,
}

impl<'input> Parameter<'input> {
    fn parse_dwarf<'state, 'abbrev, 'unit, 'tree, Endian>(
        dwarf: &DwarfFileState<'input, Endian>,
        _unit: &mut DwarfUnitState<'state, 'input, Endian>,
        _namespace: &Rc<Namespace<'input>>,
        mut iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
    ) -> Result<Parameter<'input>>
        where Endian: gimli::Endianity
    {
        let mut parameter = Parameter::default();

        {
            let entry = iter.entry().unwrap();
            let mut attrs = entry.attrs();
            while let Some(attr) = attrs.next()? {
                match attr.name() {
                    gimli::DW_AT_name => {
                        parameter.name = attr.string_value(&dwarf.debug_str);
                    }
                    gimli::DW_AT_type => {
                        if let gimli::AttributeValue::UnitRef(offset) = attr.value() {
                            parameter.ty = Some(offset);
                        }
                    }
                    gimli::DW_AT_decl_file |
                    gimli::DW_AT_decl_line |
                    gimli::DW_AT_location |
                    gimli::DW_AT_abstract_origin => {}
                    _ => {
                        debug!("unknown parameter attribute: {} {:?}",
                               attr.name(),
                               attr.value())
                    }
                }
            }
        }

        while let Some(child) = iter.next()? {
            match child.entry().unwrap().tag() {
                tag => {
                    debug!("unknown parameter child tag: {}", tag);
                }
            }
        }
        Ok(parameter)
    }

    fn bit_size(&self, state: &PrintState) -> Option<u64> {
        self.ty
            .and_then(|t| Type::from_offset(state, t))
            .and_then(|t| t.bit_size(state))
    }

    fn print(&self, w: &mut Write, state: &PrintState) -> Result<()> {
        if let Some(name) = self.name {
            write!(w, "{}: ", name.to_string_lossy())?;
        }
        match self.ty {
            Some(offset) => Type::print_offset_name(w, state, offset)?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }
}

fn parse_dwarf_lexical_block<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    inlined_subroutines: &mut Vec<InlinedSubroutine<'input>>,
    variables: &mut Vec<Variable<'input>>,
    dwarf: &DwarfFileState<'input, Endian>,
    unit: &mut DwarfUnitState<'state, 'input, Endian>,
    namespace: &Rc<Namespace<'input>>,
    mut iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
) -> Result<()>
    where Endian: gimli::Endianity
{
    {
        let entry = iter.entry().unwrap();
        let mut attrs = entry.attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_low_pc |
                gimli::DW_AT_high_pc |
                gimli::DW_AT_ranges |
                gimli::DW_AT_sibling => {}
                _ => {
                    debug!("unknown lexical_block attribute: {} {:?}",
                           attr.name(),
                           attr.value())
                }
            }
        }
    }

    while let Some(child) = iter.next()? {
        match child.entry().unwrap().tag() {
            gimli::DW_TAG_inlined_subroutine => {
                inlined_subroutines
                    .push(InlinedSubroutine::parse_dwarf(dwarf, unit, namespace, child)?);
            }
            gimli::DW_TAG_variable => {
                variables.push(Variable::parse_dwarf(dwarf, unit, namespace, child)?);
            }
            gimli::DW_TAG_lexical_block => {
                parse_dwarf_lexical_block(inlined_subroutines,
                                          variables,
                                          dwarf,
                                          unit,
                                          namespace,
                                          child)?;
            }
            tag => {
                debug!("unknown lexical_block child tag: {}", tag);
            }
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct InlinedSubroutine<'input> {
    abstract_origin: Option<gimli::UnitOffset>,
    size: Option<u64>,
    inlined_subroutines: Vec<InlinedSubroutine<'input>>,
    variables: Vec<Variable<'input>>,
}

impl<'input> InlinedSubroutine<'input> {
    fn parse_dwarf<'state, 'abbrev, 'unit, 'tree, Endian>(
        dwarf: &DwarfFileState<'input, Endian>,
        unit: &mut DwarfUnitState<'state, 'input, Endian>,
        namespace: &Rc<Namespace<'input>>,
        mut iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
    ) -> Result<InlinedSubroutine<'input>>
        where Endian: gimli::Endianity
    {
        let mut subroutine = InlinedSubroutine::default();
        let mut low_pc = None;
        let mut high_pc = None;
        let mut size = None;
        let mut ranges = None;
        {
            let entry = iter.entry().unwrap();
            let mut attrs = entry.attrs();
            while let Some(attr) = attrs.next()? {
                match attr.name() {
                    gimli::DW_AT_abstract_origin => {
                        if let gimli::AttributeValue::UnitRef(offset) = attr.value() {
                            subroutine.abstract_origin = Some(offset);
                        }
                    }
                    gimli::DW_AT_low_pc => {
                        if let gimli::AttributeValue::Addr(addr) = attr.value() {
                            low_pc = Some(addr);
                        }
                    }
                    gimli::DW_AT_high_pc => {
                        match attr.value() {
                            gimli::AttributeValue::Addr(addr) => high_pc = Some(addr),
                            gimli::AttributeValue::Udata(val) => size = Some(val),
                            _ => {}
                        }
                    }
                    gimli::DW_AT_ranges => {
                        if let gimli::AttributeValue::DebugRangesRef(val) = attr.value() {
                            ranges = Some(val);
                        }
                    }
                    gimli::DW_AT_call_file |
                    gimli::DW_AT_call_line |
                    gimli::DW_AT_sibling => {}
                    _ => {
                        debug!("unknown inlined_subroutine attribute: {} {:?}",
                               attr.name(),
                               attr.value())
                    }
                }
            }
        }

        if let Some(offset) = ranges {
            let mut size = 0;
            let low_pc = low_pc.unwrap_or(0);
            let mut ranges = dwarf.debug_ranges
                .ranges(offset, unit.header.address_size(), low_pc)?;
            while let Some(range) = ranges.next()? {
                size += range.end.wrapping_sub(range.begin);
            }
            subroutine.size = Some(size);
        } else if let Some(size) = size {
            subroutine.size = Some(size);
        } else if let (Some(low_pc), Some(high_pc)) = (low_pc, high_pc) {
            subroutine.size = Some(high_pc.wrapping_sub(low_pc));
        } else {
            debug!("unknown inlined_subroutine size");
        }

        while let Some(child) = iter.next()? {
            match child.entry().unwrap().tag() {
                gimli::DW_TAG_inlined_subroutine => {
                    subroutine.inlined_subroutines
                        .push(InlinedSubroutine::parse_dwarf(dwarf, unit, namespace, child)?);
                }
                gimli::DW_TAG_lexical_block => {
                    parse_dwarf_lexical_block(&mut subroutine.inlined_subroutines,
                                              &mut subroutine.variables,
                                              dwarf,
                                              unit,
                                              namespace,
                                              child)?;
                }
                gimli::DW_TAG_formal_parameter => {}
                tag => {
                    debug!("unknown inlined_subroutine child tag: {}", tag);
                }
            }
        }
        Ok(subroutine)
    }

    fn print(&self, w: &mut Write, state: &mut PrintState, depth: usize) -> Result<()> {
        state.line_start(w)?;
        match self.size {
            Some(size) => write!(w, "[{}]", size)?,
            None => write!(w, "[??]")?,
        }
        write!(w, "\t")?;
        match self.abstract_origin.and_then(|v| Subprogram::from_offset(state, v)) {
            Some(subprogram) => subprogram.print_name(w)?,
            None => write!(w, "<anon>")?,
        }
        writeln!(w, "")?;

        if state.flags.inline_depth > depth {
            state.indent(|state| {
                    for subroutine in &self.inlined_subroutines {
                        subroutine.print(w, state, depth + 1)?;
                    }
                    Ok(())
                })?;
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Variable<'input> {
    namespace: Rc<Namespace<'input>>,
    name: Option<&'input ffi::CStr>,
    linkage_name: Option<&'input ffi::CStr>,
    ty: Option<gimli::UnitOffset>,
    declaration: bool,
}

impl<'input> Variable<'input> {
    fn parse_dwarf<'state, 'abbrev, 'unit, 'tree, Endian>(
        dwarf: &DwarfFileState<'input, Endian>,
        _unit: &mut DwarfUnitState<'state, 'input, Endian>,
        namespace: &Rc<Namespace<'input>>,
        mut iter: gimli::EntriesTreeIter<'input, 'abbrev, 'unit, 'tree, Endian>
    ) -> Result<Variable<'input>>
        where Endian: gimli::Endianity
    {
        let mut variable = Variable::default();
        variable.namespace = namespace.clone();

        {
            let entry = iter.entry().unwrap();
            let mut attrs = entry.attrs();
            while let Some(attr) = attrs.next()? {
                match attr.name() {
                    gimli::DW_AT_name => {
                        variable.name = attr.string_value(&dwarf.debug_str);
                    }
                    gimli::DW_AT_linkage_name => {
                        variable.linkage_name = attr.string_value(&dwarf.debug_str);
                    }
                    gimli::DW_AT_type => {
                        if let gimli::AttributeValue::UnitRef(offset) = attr.value() {
                            variable.ty = Some(offset);
                        }
                    }
                    gimli::DW_AT_declaration => {
                        if let gimli::AttributeValue::Flag(flag) = attr.value() {
                            variable.declaration = flag;
                        }
                    }
                    gimli::DW_AT_abstract_origin |
                    gimli::DW_AT_artificial |
                    gimli::DW_AT_const_value |
                    gimli::DW_AT_location |
                    gimli::DW_AT_external |
                    gimli::DW_AT_decl_file |
                    gimli::DW_AT_decl_line => {}
                    _ => {
                        debug!("unknown variable attribute: {} {:?}",
                               attr.name(),
                               attr.value())
                    }
                }
            }
        }

        while let Some(child) = iter.next()? {
            match child.entry().unwrap().tag() {
                tag => {
                    debug!("unknown variable child tag: {}", tag);
                }
            }
        }
        Ok(variable)
    }

    fn bit_size(&self, state: &PrintState) -> Option<u64> {
        self.ty
            .and_then(|t| Type::from_offset(state, t))
            .and_then(|t| t.bit_size(state))
    }

    fn print(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        if !state.filter_name(self.name) || !state.filter_namespace(&*self.namespace) {
            return Ok(());
        }

        state.line_start(w)?;
        write!(w, "var ")?;
        self.print_name(w)?;
        write!(w, ": ")?;
        match self.ty {
            Some(offset) => Type::print_offset_name(w, state, offset)?,
            None => write!(w, "<anon>")?,
        }
        writeln!(w, "")?;

        state.indent(|state| {
            if let Some(linkage_name) = self.linkage_name {
                state.line_start(w)?;
                writeln!(w, "linkage name: {}", linkage_name.to_string_lossy())?;
            }

            if let Some(bit_size) = self.bit_size(state) {
                state.line_start(w)?;
                writeln!(w, "size: {}", format_bit(bit_size))?;
            } else if !self.declaration {
                debug!("variable with no size");
            }

            if self.declaration {
                state.line_start(w)?;
                writeln!(w, "declaration: yes")?;
            }

            writeln!(w, "")?;
            Ok(())
        })
    }

    fn print_name(&self, w: &mut Write) -> Result<()> {
        self.namespace.print(w)?;
        match self.name {
            Some(name) => write!(w, "{}", name.to_string_lossy())?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn cmp(a: &Variable, b: &Variable) -> cmp::Ordering {
        cmp_ns_and_name(&a.namespace, a.name, &b.namespace, b.name)
    }

    fn equal(a: &Variable, b: &Variable, _state: &DiffState) -> bool {
        if Self::cmp(a, b) != cmp::Ordering::Equal {
            return false;
        }
        // TODO
        true
    }

    fn diff(w: &mut Write, a: &Variable, b: &Variable, state: &mut DiffState) -> Result<()> {
        if Self::equal(a, b, state) {
            return Ok(());
        }
        // TODO
        write!(w, "- ")?;
        a.print(w, &mut state.a)?;
        write!(w, "+ ")?;
        b.print(w, &mut state.b)?;
        Ok(())
    }
}

fn disassemble(
    machine: xmas_elf::header::Machine,
    region: &panopticon::Region,
    low_pc: u64,
    high_pc: u64
) -> Vec<u64> {
    match machine {
        xmas_elf::header::Machine::X86_64 => {
            disassemble_arch::<amd64::Amd64>(region, low_pc, high_pc, amd64::Mode::Long)
        }
        _ => Vec::new(),
    }
}

fn disassemble_arch<A>(
    region: &panopticon::Region,
    low_pc: u64,
    high_pc: u64,
    cfg: A::Configuration
) -> Vec<u64>
    where A: panopticon::Architecture + Debug,
          A::Configuration: Debug
{
    let mut calls = Vec::new();
    let mut mnemonics = BTreeMap::new();
    let mut jumps = vec![low_pc];
    while let Some(addr) = jumps.pop() {
        if mnemonics.contains_key(&addr) {
            continue;
        }

        let m = match A::decode(region, addr, &cfg) {
            Ok(m) => m,
            Err(e) => {
                error!("failed to disassemble: {}", e);
                return calls;
            }
        };

        for mnemonic in m.mnemonics {
            //writeln!(w, "\t{:?}", mnemonic);
            /*
            write!(w, "\t{}", mnemonic.opcode);
            let mut first = true;
            for operand in &mnemonic.operands {
                if first {
                    write!(w, "\t");
                    first = false;
                } else {
                    write!(w, ", ");
                }
                match *operand {
                    panopticon::Rvalue::Variable { ref name, .. } => write!(w, "{}", name),
                    panopticon::Rvalue::Constant { ref value, .. } => write!(w, "0x{:x}", value),
                    _ => write!(w, "?"),
                }
            }
            writeln!(w, "");
            */

            for instruction in &mnemonic.instructions {
                match *instruction {
                    panopticon::Statement { op: panopticon::Operation::Call(ref call), .. } => {
                        match *call {
                            panopticon::Rvalue::Constant { ref value, .. } => {
                                calls.push(*value);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            // FIXME: mnemonic is large, insert boxed value
            mnemonics.insert(mnemonic.area.start, mnemonic);
        }

        for (_origin, target, _guard) in m.jumps {
            if let panopticon::Rvalue::Constant { value, .. } = target {
                if value > addr && value < high_pc {
                    jumps.push(value);
                }
            }
        }
    }
    calls
}

fn format_bit(val: u64) -> String {
    let byte = val / 8;
    let bit = val % 8;
    if bit == 0 {
        format!("{}", byte)
    } else {
        format!("{}.{}", byte, bit)
    }
}

#[derive(Debug)]
struct SimpleContext;

impl<'input> gimli::EvaluationContext<'input> for SimpleContext {
    type ContextError = Error;

    fn read_memory(&self, _addr: u64, _nbytes: u8, _space: Option<u64>) -> Result<u64> {
        Err("unsupported evalation read_memory callback".into())
    }
    fn read_register(&self, _regno: u64) -> Result<u64> {
        Err("unsupported evalation read_register callback".into())
    }
    fn frame_base(&self) -> Result<u64> {
        Err("unsupported evalation frame_base callback".into())
    }
    fn read_tls(&self, _slot: u64) -> Result<u64> {
        Err("unsupported evalation read_tls callback".into())
    }
    fn call_frame_cfa(&self) -> Result<u64> {
        Err("unsupported evalation call_frame_cfa callback".into())
    }
    fn get_at_location(&self, _die: gimli::DieReference) -> Result<&'input [u8]> {
        Err("unsupported evalation get_at_location callback".into())
    }
    fn evaluate_entry_value(&self, _expression: &[u8]) -> Result<u64> {
        Err("unsupported evalation evaluate_entry_value callback".into())
    }
}


fn evaluate<'input, Endian>(
    unit: &gimli::CompilationUnitHeader<Endian>,
    bytecode: &'input [u8]
) -> Result<gimli::Location<'input>>
    where Endian: gimli::Endianity
{
    let mut context = SimpleContext {};
    let mut evaluation = gimli::Evaluation::<Endian, SimpleContext>::new(bytecode,
                                                                         unit.address_size(),
                                                                         unit.format(),
                                                                         &mut context);
    evaluation.set_initial_value(0);
    let pieces = evaluation.evaluate()?;
    if pieces.len() != 1 {
        return Err(format!("unsupported number of evaluation pieces: {}", pieces.len()).into());
    }
    Ok(pieces[0].location)
}
