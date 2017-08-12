extern crate gimli;
#[macro_use]
extern crate log;
extern crate memmap;
extern crate goblin;
extern crate panopticon_core as panopticon;
extern crate panopticon_amd64 as amd64;
extern crate pdb as crate_pdb;

use std::borrow::Borrow;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::cmp;
use std::error;
use std::fmt::{self, Debug};
use std::fs;
use std::io;
use std::io::Write;
use std::result;
use std::rc::Rc;

mod diff;
mod dwarf;
mod pdb;

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

impl From<crate_pdb::Error> for Error {
    fn from(e: crate_pdb::Error) -> Error {
        Error(Cow::Owned(format!("PDB error: {}", e)))
    }
}

pub type Result<T> = result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    None,
    Name,
    Size,
}

impl Sort {
    fn with_diff(self, diff: bool) -> Self {
        if diff { Sort::Name } else { self }
    }
}

impl Default for Sort {
    fn default() -> Self {
        Sort::None
    }
}

#[derive(Debug, Default, Clone)]
pub struct Flags<'a> {
    pub calls: bool,
    pub sort: Sort,
    pub ignore_added: bool,
    pub ignore_deleted: bool,
    pub ignore_function_address: bool,
    pub ignore_function_size: bool,
    pub ignore_function_inline: bool,
    pub ignore_variable_address: bool,
    pub inline_depth: usize,
    pub unit: Option<&'a str>,
    pub name: Option<&'a str>,
    pub namespace: Vec<&'a str>,
}

impl<'a> Flags<'a> {
    pub fn unit(&mut self, unit: &'a str) -> &mut Self {
        self.unit = Some(unit);
        self
    }

    pub fn name(&mut self, name: &'a str) -> &mut Self {
        self.name = Some(name);
        self
    }

    fn filter_unit(&self, unit: Option<&[u8]>) -> bool {
        if let Some(filter) = self.unit {
            filter_name(unit, filter)
        } else {
            true
        }
    }

    fn filter_name(&self, name: Option<&[u8]>) -> bool {
        if let Some(filter) = self.name {
            filter_name(name, filter)
        } else {
            true
        }
    }

    fn filter_namespace(&self, namespace: &Option<Rc<Namespace>>) -> bool {
        if !self.namespace.is_empty() {
            match *namespace {
                Some(ref namespace) => namespace.filter(&self.namespace),
                None => false,
            }
        } else {
            true
        }
    }
}

fn filter_name(name: Option<&[u8]>, filter: &str) -> bool {
    match name {
        Some(name) => name == filter.as_bytes(),
        None => false,
    }
}

fn filter_option<T, F>(o: Option<T>, f: F) -> Option<T>
    where T: Copy,
          F: FnOnce(T) -> bool
{
    o.and_then(|v| if f(v) { Some(v) } else { None })
}

#[derive(Debug)]
pub struct CodeRegion {
    // TODO: use format independent machine type
    machine: u16,
    region: panopticon::Region,
}

#[derive(Debug)]
pub struct File<'input> {
    code: Option<CodeRegion>,
    units: Vec<Unit<'input>>,
}

impl<'input> File<'input> {
    fn filter_units(&self, flags: &Flags, diff: bool) -> Vec<&Unit> {
        let mut units: Vec<_> = self.units.iter().filter(|a| a.filter(flags)).collect();
        match flags.sort.with_diff(diff) {
            Sort::None => {}
            Sort::Name => units.sort_by(|a, b| Unit::cmp_id(a, b)),
            Sort::Size => units.sort_by(|a, b| Unit::cmp_size(a, b)),
        }
        units
    }
}

pub fn parse_file(path: &str, cb: &mut FnMut(&mut File) -> Result<()>) -> Result<()> {
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
    if input.starts_with(b"Microsoft C/C++ MSF 7.00\r\n\x1a\x44\x53\x00") {
        pdb::parse(input, cb)
    } else {
        let mut cursor = io::Cursor::new(input);
        match goblin::peek(&mut cursor) {
            Ok(goblin::Hint::Elf(_)) => parse_elf(input, cb),
            Ok(goblin::Hint::Mach(_)) => parse_mach(input, cb),
            Ok(_) => Err("unrecognized file format".into()),
            Err(e) => Err(format!("file identification failed: {}", e).into()),
        }
    }
}

pub fn parse_elf<'input>(
    input: &'input [u8],
    cb: &mut FnMut(&mut File) -> Result<()>,
) -> Result<()> {
    let elf = match goblin::elf::Elf::parse(&input) {
        Ok(elf) => elf,
        Err(e) => return Err(format!("ELF parse failed: {}", e).into()),
    };

    let machine = elf.header.e_machine;
    let region = match machine {
        goblin::elf::header::EM_X86_64 => {
            Some(panopticon::Region::undefined("RAM".to_string(), 0xFFFF_FFFF_FFFF_FFFF))
        }
        _ => None,
    };

    let mut code = None;
    if let Some(mut region) = region {
        for ph in &elf.program_headers {
            if ph.p_type == goblin::elf::program_header::PT_LOAD {
                let offset = ph.p_offset;
                let size = ph.p_filesz;
                let addr = ph.p_vaddr;
                if offset as usize <= input.len() {
                    let input = &input[offset as usize..];
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
        code = Some(CodeRegion { machine, region });
    }

    // Code based on 'object' crate
    let get_section = |section_name: &str| -> &'input [u8] {
        for header in &elf.section_headers {
            if let Ok(name) = elf.shdr_strtab.get(header.sh_name) {
                if name == section_name {
                    return &input[header.sh_offset as usize..][..header.sh_size as usize];
                }
            }
        }
        &[]
    };

    let units = match elf.header.e_ident[goblin::elf::header::EI_DATA] {
        goblin::elf::header::ELFDATA2LSB => dwarf::parse(gimli::LittleEndian, get_section)?,
        goblin::elf::header::ELFDATA2MSB => dwarf::parse(gimli::BigEndian, get_section)?,
        _ => return Err("unknown endianity".into()),
    };

    let mut file = File { code, units };
    cb(&mut file)
}

pub fn parse_mach<'input>(
    input: &'input [u8],
    cb: &mut FnMut(&mut File) -> Result<()>,
) -> Result<()> {
    let macho = match goblin::mach::MachO::parse(&input, 0) {
        Ok(macho) => macho,
        Err(e) => return Err(format!("Mach-O parse failed: {}", e).into()),
    };

    // Code based on 'object' crate
    let get_section = |section_name: &str| -> &'input [u8] {
        let mut name = Vec::with_capacity(section_name.len() + 1);
        name.push(b'_');
        name.push(b'_');
        for ch in &section_name.as_bytes()[1..] {
            name.push(*ch);
        }
        let section_name = name;

        for segment in &*macho.segments {
            if let Ok(name) = segment.name() {
                if name == "__DWARF" {
                    if let Ok(sections) = segment.sections() {
                        for section in sections {
                            if let Ok(name) = section.name() {
                                if name.as_bytes() == &*section_name {
                                    return section.data;
                                }
                            }
                        }
                    }
                }
            }
        }
        &[]
    };

    let units = if macho.header.is_little_endian() {
        dwarf::parse(gimli::LittleEndian, get_section)?
    } else {
        dwarf::parse(gimli::BigEndian, get_section)?
    };

    let mut file = File { code: None, units };
    cb(&mut file)
}

#[derive(Debug)]
struct FileHash<'a, 'input>
    where 'input: 'a
{
    // All subprograms by address.
    subprograms: HashMap<u64, &'a Subprogram<'input>>,
    // All types by offset.
    types: HashMap<TypeOffset, &'a Type<'input>>,
}

impl<'a, 'input> FileHash<'a, 'input> {
    fn new(file: &'a File<'input>) -> Self {
        FileHash {
            subprograms: Self::subprograms(file),
            types: Self::types(file),
        }
    }

    /// Returns a map from address to subprogram for all subprograms in the file.
    fn subprograms(file: &'a File<'input>) -> HashMap<u64, &'a Subprogram<'input>> {
        let mut subprograms = HashMap::new();
        // TODO: insert symbol table names too
        for unit in &file.units {
            for subprogram in unit.subprograms.values() {
                if let Some(low_pc) = subprogram.low_pc {
                    // TODO: handle duplicate addresses
                    subprograms.insert(low_pc, subprogram);
                }
            }
        }
        subprograms
    }

    /// Returns a map from offset to type for all types in the file.
    fn types(file: &'a File<'input>) -> HashMap<TypeOffset, &'a Type<'input>> {
        let mut types = HashMap::new();
        for unit in &file.units {
            for (offset, ty) in unit.types.iter() {
                types.insert(*offset, ty);
            }
        }
        types
    }
}

#[derive(Debug, Clone, Copy)]
enum DiffPrefix {
    None,
    Equal,
    Less,
    Greater,
}

struct PrintState<'a, 'input>
    where 'input: 'a
{
    indent: usize,
    prefix: DiffPrefix,
    // True if DiffPrefix::Less or DiffPrefix::Greater was printed.
    diff: bool,

    // The remaining fields contain information that is commonly needed in print methods.
    file: &'a File<'input>,
    hash: &'a FileHash<'a, 'input>,
    flags: &'a Flags<'a>,
}

impl<'a, 'input> PrintState<'a, 'input>
    where 'input: 'a
{
    fn new(file: &'a File<'input>, hash: &'a FileHash<'a, 'input>, flags: &'a Flags<'a>) -> Self {
        PrintState {
            indent: 0,
            prefix: DiffPrefix::None,
            diff: false,
            file: file,
            hash: hash,
            flags: flags,
        }
    }

    fn indent<F>(&mut self, mut f: F) -> Result<()>
        where F: FnMut(&mut PrintState<'a, 'input>) -> Result<()>
    {
        self.indent += 1;
        let ret = f(self);
        self.indent -= 1;
        ret
    }

    fn prefix<F>(&mut self, prefix: DiffPrefix, mut f: F) -> Result<()>
        where F: FnMut(&mut PrintState<'a, 'input>) -> Result<()>
    {
        let prev = self.prefix;
        self.prefix = prefix;
        let ret = f(self);
        self.prefix = prev;
        ret
    }

    fn line<F>(&mut self, w: &mut Write, mut f: F) -> Result<()>
        where F: FnMut(&mut Write, &mut PrintState<'a, 'input>) -> Result<()>
    {
        match self.prefix {
            DiffPrefix::None => {}
            DiffPrefix::Equal => write!(w, "  ")?,
            DiffPrefix::Less => {
                write!(w, "- ")?;
                self.diff = true;
            }
            DiffPrefix::Greater => {
                write!(w, "+ ")?;
                self.diff = true;
            }
        }
        for _ in 0..self.indent {
            write!(w, "\t")?;
        }
        f(w, self)?;
        write!(w, "\n")?;
        Ok(())
    }

    fn line_option<F>(&mut self, w: &mut Write, mut f: F) -> Result<()>
        where F: FnMut(&mut Write, &mut PrintState<'a, 'input>) -> Result<()>
    {
        let mut buf = Vec::new();
        let mut state = PrintState::new(self.file, self.hash, self.flags);
        f(&mut buf, &mut state)?;
        if !buf.is_empty() {
            self.line(w, |w, _state| w.write_all(&*buf).map_err(From::from))?;
        }
        Ok(())
    }
}

pub fn print_file(w: &mut Write, file: &File, flags: &Flags) -> Result<()> {
    let hash = FileHash::new(file);
    for unit in &file.filter_units(flags, false) {
        let mut state = PrintState::new(file, &hash, flags);
        if flags.unit.is_none() {
            state.line(w, |w, _state| {
                write!(w, "Unit: ")?;
                unit.print_ref(w)
            })?;
        }
        unit.print(w, &mut state, flags)?;
    }
    Ok(())
}

enum MergeResult<T> {
    Left(T),
    Right(T),
    Both(T, T),
}

struct MergeIterator<T, I, C>
    where T: Copy,
          I: Iterator<Item = T>,
          C: Fn(T, T) -> cmp::Ordering
{
    iter_left: I,
    iter_right: I,
    item_left: Option<T>,
    item_right: Option<T>,
    item_cmp: C,
}

impl<T, I, C> MergeIterator<T, I, C>
    where T: Copy,
          I: Iterator<Item = T>,
          C: Fn(T, T) -> cmp::Ordering
{
    fn new(mut left: I, mut right: I, cmp: C) -> Self {
        let item_left = left.next();
        let item_right = right.next();
        MergeIterator {
            iter_left: left,
            iter_right: right,
            item_left: item_left,
            item_right: item_right,
            item_cmp: cmp,
        }
    }
}

impl<T, I, C> Iterator for MergeIterator<T, I, C>
    where T: Copy,
          I: Iterator<Item = T>,
          C: Fn(T, T) -> cmp::Ordering
{
    type Item = MergeResult<T>;

    fn next(&mut self) -> Option<MergeResult<T>> {
        match (self.item_left, self.item_right) {
            (Some(left), Some(right)) => {
                match (self.item_cmp)(left, right) {
                    cmp::Ordering::Equal => {
                        self.item_left = self.iter_left.next();
                        self.item_right = self.iter_right.next();
                        Some(MergeResult::Both(left, right))
                    }
                    cmp::Ordering::Less => {
                        self.item_left = self.iter_left.next();
                        Some(MergeResult::Left(left))
                    }
                    cmp::Ordering::Greater => {
                        self.item_right = self.iter_right.next();
                        Some(MergeResult::Right(right))
                    }
                }
            }
            (Some(left), None) => {
                self.item_left = self.iter_left.next();
                Some(MergeResult::Left(left))
            }
            (None, Some(right)) => {
                self.item_right = self.iter_right.next();
                Some(MergeResult::Right(right))
            }
            (None, None) => None,
        }
    }
}

struct DiffState<'a, 'input>
    where 'input: 'a
{
    a: PrintState<'a, 'input>,
    b: PrintState<'a, 'input>,
    flags: &'a Flags<'a>,
}

impl<'a, 'input> DiffState<'a, 'input>
    where 'input: 'a
{
    fn new(
        file_a: &'a File<'input>,
        hash_a: &'a FileHash<'a, 'input>,
        file_b: &'a File<'input>,
        hash_b: &'a FileHash<'a, 'input>,
        flags: &'a Flags<'a>,
    ) -> Self {
        DiffState {
            a: PrintState::new(file_a, hash_a, flags),
            b: PrintState::new(file_b, hash_b, flags),
            flags: flags,
        }
    }

    fn merge<T, I, FIterA, FIterB, FCmp, FEqual, FLess, FGreater>(
        &mut self,
        w: &mut Write,
        iter_a: FIterA,
        iter_b: FIterB,
        cmp: FCmp,
        mut equal: FEqual,
        less: FLess,
        greater: FGreater,
    ) -> Result<()>
        where T: Copy,
              I: IntoIterator<Item = T>,
              FIterA: Fn(&PrintState<'a, 'input>) -> I,
              FIterB: Fn(&PrintState<'a, 'input>) -> I,
              FCmp: Fn(&FileHash, T, &FileHash, T) -> cmp::Ordering,
              FEqual: FnMut(&mut Write, &mut DiffState<'a, 'input>, T, T) -> Result<()>,
              FLess: Fn(&mut Write, &mut PrintState<'a, 'input>, T) -> Result<()>,
              FGreater: Fn(&mut Write, &mut PrintState<'a, 'input>, T) -> Result<()>
    {
        let iter_a = &mut iter_a(&self.a).into_iter();
        let iter_b = &mut iter_b(&self.b).into_iter();
        let hash_a = self.a.hash;
        let hash_b = self.b.hash;
        for m in MergeIterator::new(iter_a, iter_b, |a, b| cmp(hash_a, a, hash_b, b)) {
            match m {
                MergeResult::Both(l, r) => self.prefix_equal(|state| equal(w, state, l, r))?,
                MergeResult::Left(l) => self.prefix_less(|state| less(w, state, l))?,
                MergeResult::Right(r) => self.prefix_greater(|state| greater(w, state, r))?,
            }
        }
        Ok(())
    }

    fn diff<F>(&mut self, w: &mut Write, mut f: F) -> Result<()>
        where F: FnMut(&mut Write, &mut DiffState<'a, 'input>) -> Result<()>
    {
        let mut buf = Vec::new();
        self.a.diff = false;
        self.b.diff = false;
        f(&mut buf, self)?;
        if self.a.diff || self.b.diff {
            w.write_all(&*buf)?;
        }
        Ok(())
    }

    fn ignore_diff<F>(&mut self, flag: bool, mut f: F) -> Result<()>
        where F: FnMut(&mut DiffState<'a, 'input>) -> Result<()>
    {
        let a_diff = self.a.diff;
        let b_diff = self.b.diff;
        f(self)?;
        if flag {
            self.a.diff = a_diff;
            self.b.diff = b_diff;
        }
        Ok(())
    }

    fn indent<F>(&mut self, mut f: F) -> Result<()>
        where F: FnMut(&mut DiffState<'a, 'input>) -> Result<()>
    {
        self.a.indent += 1;
        self.b.indent += 1;
        let ret = f(self);
        self.a.indent -= 1;
        self.b.indent -= 1;
        ret
    }

    fn prefix_equal<F>(&mut self, mut f: F) -> Result<()>
        where F: FnMut(&mut DiffState<'a, 'input>) -> Result<()>
    {
        let prev_a = self.a.prefix;
        let prev_b = self.b.prefix;
        self.a.prefix = DiffPrefix::Equal;
        self.b.prefix = DiffPrefix::Equal;
        let ret = f(self);
        self.a.prefix = prev_a;
        self.b.prefix = prev_b;
        ret
    }

    fn prefix_less<F>(&mut self, f: F) -> Result<()>
        where F: FnMut(&mut PrintState<'a, 'input>) -> Result<()>
    {
        self.a.prefix(DiffPrefix::Less, f)
    }

    fn prefix_greater<F>(&mut self, f: F) -> Result<()>
        where F: FnMut(&mut PrintState<'a, 'input>) -> Result<()>
    {
        self.b.prefix(DiffPrefix::Greater, f)
    }

    fn prefix_diff<F>(&mut self, mut f: F) -> Result<()>
        where F: FnMut(&mut DiffState<'a, 'input>) -> Result<()>
    {
        let prev_a = self.a.prefix;
        let prev_b = self.b.prefix;
        self.a.prefix = DiffPrefix::Less;
        self.b.prefix = DiffPrefix::Greater;
        let ret = f(self);
        self.a.prefix = prev_a;
        self.b.prefix = prev_b;
        ret
    }

    fn line<F, T>(&mut self, w: &mut Write, arg_a: T, arg_b: T, mut f: F) -> Result<()>
        where F: FnMut(&mut Write, &mut PrintState<'a, 'input>, T) -> Result<()>
    {
        let mut a = Vec::new();
        let mut state = PrintState::new(self.a.file, self.a.hash, self.a.flags);
        f(&mut a, &mut state, arg_a)?;

        let mut b = Vec::new();
        let mut state = PrintState::new(self.b.file, self.b.hash, self.b.flags);
        f(&mut b, &mut state, arg_b)?;

        if a == b {
            self.prefix_equal(|state| {
                if !a.is_empty() {
                    state.a.line(w, |w, _state| w.write_all(&*a).map_err(From::from))?;
                }
                Ok(())
            })
        } else {
            self.prefix_diff(|state| {
                if !a.is_empty() {
                    state.a.line(w, |w, _state| w.write_all(&*a).map_err(From::from))?;
                }
                if !b.is_empty() {
                    state.b.line(w, |w, _state| w.write_all(&*b).map_err(From::from))?;
                }
                Ok(())
            })
        }
    }

    /// This is the same as `Self::line`. It exists for symmetry with `PrintState::line_option`.
    fn line_option<F, T>(&mut self, w: &mut Write, arg_a: T, arg_b: T, f: F) -> Result<()>
        where F: FnMut(&mut Write, &mut PrintState<'a, 'input>, T) -> Result<()>
    {
        self.line(w, arg_a, arg_b, f)
    }

    fn list<T, Cost, Arg, MutArg, Print, Diff>(
        &mut self,
        w: &mut Write,
        list_a: &[T],
        arg_a: Arg,
        mut_arg_a: &mut MutArg,
        list_b: &[T],
        arg_b: Arg,
        mut_arg_b: &mut MutArg,
        step_cost: usize,
        diff_cost: Cost,
        mut print: Print,
        mut diff: Diff,
    ) -> Result<()>
        where Arg: Copy,
              Cost: Fn(&DiffState<'a, 'input>, &T, &T) -> usize,
              Print: FnMut(&mut Write, &mut PrintState<'a, 'input>, &T, Arg, &mut MutArg)
                           -> Result<()>,
              Diff: FnMut(&mut Write,
                          &mut DiffState<'a, 'input>,
                          &T,
                          Arg,
                          &mut MutArg,
                          &T,
                          Arg,
                          &mut MutArg)
                          -> Result<()>
    {
        let path = diff::shortest_path(list_a, list_b, step_cost, |a, b| diff_cost(self, a, b));
        let mut iter_a = list_a.iter();
        let mut iter_b = list_b.iter();
        for dir in path {
            match dir {
                diff::Direction::None => break,
                diff::Direction::Diagonal => {
                    if let (Some(a), Some(b)) = (iter_a.next(), iter_b.next()) {
                        diff(w, self, a, arg_a, mut_arg_a, b, arg_b, mut_arg_b)?;
                    }
                }
                diff::Direction::Horizontal => {
                    if let Some(a) = iter_a.next() {
                        self.prefix_less(|state| print(w, state, a, arg_a, mut_arg_a))?;
                    }
                }
                diff::Direction::Vertical => {
                    if let Some(b) = iter_b.next() {
                        self.prefix_greater(|state| print(w, state, b, arg_b, mut_arg_b))?;
                    }
                }
            }
        }
        Ok(())
    }
}

pub fn diff_file(w: &mut Write, file_a: &File, file_b: &File, flags: &Flags) -> Result<()> {
    let hash_a = FileHash::new(file_a);
    let hash_b = FileHash::new(file_b);
    let mut state = DiffState::new(file_a, &hash_a, file_b, &hash_b, flags);
    state
        .merge(
            w,
            |_state| file_a.filter_units(flags, true),
            |_state| file_b.filter_units(flags, true),
            |_hash_a, a, _hash_b, b| Unit::cmp_id(a, b),
            |w, state, a, b| {
                if flags.unit.is_none() {
                    state
                        .a
                        .line(
                            w, |w, _state| {
                                write!(w, "Unit: ")?;
                                a.print_ref(w)
                            }
                        )?;
                }
                Unit::diff(a, b, w, state, flags)
            },
            |w, state, a| {
                state.line(
                    w, |w, _state| {
                        write!(w, "Unit: ")?;
                        a.print_ref(w)
                    }
                )
            },
            |w, state, b| {
                state.line(
                    w, |w, _state| {
                        write!(w, "Unit: ")?;
                        b.print_ref(w)
                    }
                )
            },
        )?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum NamespaceKind {
    Namespace,
    Subprogram,
    Type,
}

#[derive(Debug)]
struct Namespace<'input> {
    parent: Option<Rc<Namespace<'input>>>,
    name: Option<&'input [u8]>,
    kind: NamespaceKind,
}

impl<'input> Namespace<'input> {
    fn new(
        parent: &Option<Rc<Namespace<'input>>>,
        name: Option<&'input [u8]>,
        kind: NamespaceKind,
    ) -> Rc<Namespace<'input>> {
        Rc::new(Namespace {
            parent: parent.clone(),
            name: name,
            kind: kind,
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
        }
        match self.name {
            Some(name) => write!(w, "{}", String::from_utf8_lossy(name))?,
            None => write!(w, "<anon>")?,
        }
        write!(w, "::")?;
        Ok(())
    }

    fn _filter(&self, namespace: &[&str]) -> (bool, usize) {
        let (ret, offset) = match self.parent {
            Some(ref parent) => parent._filter(namespace),
            None => (true, 0),
        };

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

    fn filter(&self, namespace: &[&str]) -> bool {
        self._filter(namespace) == (true, namespace.len())
    }

    fn _cmp(a: &Namespace, b: &Namespace) -> cmp::Ordering {
        debug_assert_eq!(a.len(), b.len());
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

    fn is_anon_type(namespace: &Option<Rc<Namespace>>) -> bool {
        match *namespace {
            Some(ref namespace) => {
                namespace.kind == NamespaceKind::Type &&
                (namespace.name.is_none() || Namespace::is_anon_type(&namespace.parent))
            }
            None => false,
        }
    }
}

fn cmp_ns_and_name(
    ns1: &Option<Rc<Namespace>>,
    name1: Option<&[u8]>,
    ns2: &Option<Rc<Namespace>>,
    name2: Option<&[u8]>,
) -> cmp::Ordering {
    match (ns1, ns2) {
        (&Some(ref ns1), &Some(ref ns2)) => {
            match Namespace::cmp(ns1, ns2) {
                cmp::Ordering::Equal => name1.cmp(&name2),
                o => o,
            }
        }
        (&Some(_), &None) => cmp::Ordering::Greater,
        (&None, &Some(_)) => cmp::Ordering::Less,
        (&None, &None) => name1.cmp(&name2),
    }
}

#[derive(Debug, Default)]
pub struct Unit<'input> {
    dir: Option<&'input [u8]>,
    name: Option<&'input [u8]>,
    language: Option<gimli::DwLang>,
    address_size: Option<u64>,
    low_pc: Option<u64>,
    high_pc: Option<u64>,
    size: Option<u64>,
    types: BTreeMap<TypeOffset, Type<'input>>,
    subprograms: BTreeMap<SubprogramOffset, Subprogram<'input>>,
    variables: BTreeMap<VariableOffset, Variable<'input>>,
}

impl<'input> Unit<'input> {
    /// Return true if this unit matches the filter options in the flags.
    fn filter(&self, flags: &Flags) -> bool {
        flags.filter_unit(self.name)
    }

    /// The offsets of types that should be printed inline.
    fn inline_types(&self, state: &PrintState) -> HashSet<usize> {
        let mut inline_types = HashSet::new();
        for ty in self.types.values() {
            // Assume all anonymous types are inline. We don't actually check
            // that they will be inline, but in future we could (eg for TypeDefs).
            // TODO: is this a valid assumption?
            if ty.is_anon() {
                inline_types.insert(ty.offset.0);
            }

            // Find all inline members.
            ty.visit_members(&mut |t| if t.is_inline(state.hash) {
                if let Some(offset) = t.ty {
                    inline_types.insert(offset.0);
                }
            });
        }
        inline_types
    }

    /// Filter and sort the list of types using the options in the flags.
    /// Perform additional filtering and always sort when diffing.
    fn filter_types(&self, state: &PrintState, flags: &Flags, diff: bool) -> Vec<&Type> {
        let inline_types = self.inline_types(state);
        let filter_type = |t: &Type| {
            // Filter by user options.
            if !t.filter(flags) {
                return false;
            }
            match t.kind {
                TypeKind::Struct(ref t) => {
                    // Hack for rust closures
                    // TODO: is there better way of identifying these, or a
                    // a way to match pairs for diffing?
                    if diff && filter_name(t.name, "closure") {
                        return false;
                    }
                }
                TypeKind::Def(..) |
                TypeKind::Union(..) |
                TypeKind::Enumeration(..) => {}
                TypeKind::Base(..) |
                TypeKind::Array(..) |
                TypeKind::Subroutine(..) |
                TypeKind::Unspecified(..) |
                TypeKind::PointerToMember(..) |
                TypeKind::Modifier(..) => return false,
            }
            // Filter out inline types.
            !inline_types.contains(&t.offset.0)
        };
        let mut types: Vec<_> = self.types.values().filter(|a| filter_type(a)).collect();
        match flags.sort.with_diff(diff) {
            Sort::None => {}
            Sort::Name => types.sort_by(|a, b| Type::cmp_id(state.hash, a, state.hash, b)),
            Sort::Size => types.sort_by(|a, b| Type::cmp_size(state.hash, a, state.hash, b)),
        }
        types
    }

    /// Filter and sort the list of subprograms using the options in the flags.
    /// Always sort when diffing.
    fn filter_subprograms(&self, flags: &Flags, diff: bool) -> Vec<&Subprogram> {
        let mut subprograms: Vec<_> =
            self.subprograms.values().filter(|a| a.filter(flags)).collect();
        match flags.sort.with_diff(diff) {
            Sort::None => {}
            Sort::Name => subprograms.sort_by(|a, b| Subprogram::cmp_id(a, b)),
            Sort::Size => subprograms.sort_by(|a, b| Subprogram::cmp_size(a, b)),
        }
        subprograms
    }

    /// Filter and sort the list of variables using the options in the flags.
    /// Always sort when diffing.
    fn filter_variables(&self, state: &PrintState, flags: &Flags, diff: bool) -> Vec<&Variable> {
        let mut variables: Vec<_> = self.variables.values().filter(|a| a.filter(flags)).collect();
        match flags.sort.with_diff(diff) {
            Sort::None => {}
            Sort::Name => variables.sort_by(|a, b| Variable::cmp_id(a, b)),
            Sort::Size => {
                variables.sort_by(|a, b| Variable::cmp_size(state.hash, a, state.hash, b))
            }
        }
        variables
    }

    fn print_ref(&self, w: &mut Write) -> Result<()> {
        match self.name {
            Some(name) => write!(w, "{}", String::from_utf8_lossy(name))?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn print(&self, w: &mut Write, state: &mut PrintState, flags: &Flags) -> Result<()> {
        for ty in &self.filter_types(state, flags, false) {
            ty.print(w, state, self)?;
            writeln!(w, "")?;
        }
        for subprogram in &self.filter_subprograms(flags, false) {
            subprogram.print(w, state, self)?;
            writeln!(w, "")?;
        }
        for variable in &self.filter_variables(state, flags, false) {
            variable.print(w, state)?;
            writeln!(w, "")?;
        }
        Ok(())
    }

    /// Compare the identifying information of two units.
    /// This can be used to sort, and to determine if two units refer to the same source.
    fn cmp_id(a: &Unit, b: &Unit) -> cmp::Ordering {
        // TODO: ignore base paths
        a.name.cmp(&b.name)
    }

    /// Compare the size of two units.
    fn cmp_size(a: &Unit, b: &Unit) -> cmp::Ordering {
        a.size.cmp(&b.size)
    }

    fn diff(
        unit_a: &Unit,
        unit_b: &Unit,
        w: &mut Write,
        state: &mut DiffState,
        flags: &Flags,
    ) -> Result<()> {
        state
            .merge(
                w,
                |state| unit_a.filter_types(state, flags, true),
                |state| unit_b.filter_types(state, flags, true),
                |hash_a, a, hash_b, b| Type::cmp_id(hash_a, a, hash_b, b),
                |w, state, a, b| {
                    state.diff(
                        w, |w, state| {
                            Type::diff(w, state, unit_a, a, unit_b, b)?;
                            writeln!(w, "")?;
                            Ok(())
                        }
                    )
                },
                |w, state, a| {
                    if !flags.ignore_deleted {
                        a.print(w, state, unit_a)?;
                        writeln!(w, "")?;
                    }
                    Ok(())
                },
                |w, state, b| {
                    if !flags.ignore_added {
                        b.print(w, state, unit_b)?;
                        writeln!(w, "")?;
                    }
                    Ok(())
                },
            )?;
        state
            .merge(
                w,
                |_state| unit_a.filter_subprograms(flags, true),
                |_state| unit_b.filter_subprograms(flags, true),
                |_hash_a, a, _hash_b, b| Subprogram::cmp_id(a, b),
                |w, state, a, b| {
                    state.diff(
                        w, |w, state| {
                            Subprogram::diff(w, state, unit_a, a, unit_b, b)?;
                            writeln!(w, "")?;
                            Ok(())
                        }
                    )
                },
                |w, state, a| {
                    if !flags.ignore_deleted {
                        a.print(w, state, unit_a)?;
                        writeln!(w, "")?;
                    }
                    Ok(())
                },
                |w, state, b| {
                    if !flags.ignore_added {
                        b.print(w, state, unit_b)?;
                        writeln!(w, "")?;
                    }
                    Ok(())
                },
            )?;
        state
            .merge(
                w,
                |state| unit_a.filter_variables(state, flags, true),
                |state| unit_b.filter_variables(state, flags, true),
                |_hash_a, a, _hash_b, b| Variable::cmp_id(a, b),
                |w, state, a, b| {
                    state.diff(
                        w, |w, state| {
                            Variable::diff(w, state, a, b)?;
                            writeln!(w, "")?;
                            Ok(())
                        }
                    )
                },
                |w, state, a| {
                    if !flags.ignore_deleted {
                        a.print(w, state)?;
                        writeln!(w, "")?;
                    }
                    Ok(())
                },
                |w, state, b| {
                    if !flags.ignore_added {
                        b.print(w, state)?;
                        writeln!(w, "")?;
                    }
                    Ok(())
                },
            )?;
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
    Unspecified(UnspecifiedType<'input>),
    PointerToMember(PointerToMemberType),
    Modifier(TypeModifier<'input>),
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
            TypeKind::Unspecified(..) => 7,
            TypeKind::PointerToMember(..) => 8,
            TypeKind::Modifier(..) => 9,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct TypeOffset(usize);

impl From<gimli::DebugInfoOffset> for TypeOffset {
    fn from(o: gimli::DebugInfoOffset) -> TypeOffset {
        TypeOffset(o.0)
    }
}

#[derive(Debug)]
struct Type<'input> {
    offset: TypeOffset,
    kind: TypeKind<'input>,
}

impl<'input> Default for Type<'input> {
    fn default() -> Self {
        Type {
            offset: TypeOffset(0),
            kind: TypeKind::Base(BaseType::default()),
        }
    }
}

impl<'input> Type<'input> {
    fn from_offset<'a>(
        hash: &'a FileHash<'a, 'input>,
        offset: TypeOffset,
    ) -> Option<&'a Type<'input>>
        where 'input: 'a
    {
        hash.types.get(&offset).map(|ty| *ty)
    }

    fn byte_size(&self, hash: &FileHash) -> Option<u64> {
        match self.kind {
            TypeKind::Base(ref val) => val.byte_size(),
            TypeKind::Def(ref val) => val.byte_size(hash),
            TypeKind::Struct(ref val) => val.byte_size(),
            TypeKind::Union(ref val) => val.byte_size(),
            TypeKind::Enumeration(ref val) => val.byte_size(hash),
            TypeKind::Array(ref val) => val.byte_size(hash),
            TypeKind::Subroutine(ref val) => val.byte_size(),
            TypeKind::Unspecified(..) => None,
            TypeKind::PointerToMember(ref val) => val.byte_size(hash),
            TypeKind::Modifier(ref val) => val.byte_size(hash),
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
            TypeKind::Unspecified(..) |
            TypeKind::PointerToMember(..) |
            TypeKind::Modifier(..) => {}
        }
    }

    fn filter(&self, flags: &Flags) -> bool {
        match self.kind {
            TypeKind::Def(ref val) => val.filter(flags),
            TypeKind::Struct(ref val) => val.filter(flags),
            TypeKind::Union(ref val) => val.filter(flags),
            TypeKind::Enumeration(ref val) => val.filter(flags),
            TypeKind::Unspecified(ref val) => val.filter(flags),
            TypeKind::Base(..) |
            TypeKind::Array(..) |
            TypeKind::Subroutine(..) |
            TypeKind::PointerToMember(..) |
            TypeKind::Modifier(..) => flags.name.is_none(),
        }
    }

    fn print(&self, w: &mut Write, state: &mut PrintState, unit: &Unit) -> Result<()> {
        match self.kind {
            TypeKind::Def(ref val) => val.print(w, state, unit),
            TypeKind::Struct(ref val) => val.print(w, state, unit),
            TypeKind::Union(ref val) => val.print(w, state, unit),
            TypeKind::Enumeration(ref val) => val.print(w, state),
            TypeKind::Base(..) |
            TypeKind::Array(..) |
            TypeKind::Subroutine(..) |
            TypeKind::Unspecified(..) |
            TypeKind::PointerToMember(..) |
            TypeKind::Modifier(..) => Err(format!("can't print {:?}", self).into()),
        }
    }

    fn print_ref(&self, w: &mut Write, state: &PrintState) -> Result<()> {
        match self.kind {
            TypeKind::Base(ref val) => val.print_ref(w),
            TypeKind::Def(ref val) => val.print_ref(w),
            TypeKind::Struct(ref val) => val.print_ref(w),
            TypeKind::Union(ref val) => val.print_ref(w),
            TypeKind::Enumeration(ref val) => val.print_ref(w),
            TypeKind::Array(ref val) => val.print_ref(w, state),
            TypeKind::Subroutine(ref val) => val.print_ref(w, state),
            TypeKind::Unspecified(ref val) => val.print_ref(w),
            TypeKind::PointerToMember(ref val) => val.print_ref(w, state),
            TypeKind::Modifier(ref val) => val.print_ref(w, state),
        }
    }

    fn print_ref_from_offset(
        w: &mut Write,
        state: &PrintState,
        offset: Option<TypeOffset>,
    ) -> Result<()> {
        match offset {
            Some(offset) => {
                match Type::from_offset(state.hash, offset) {
                    Some(ty) => ty.print_ref(w, state)?,
                    None => write!(w, "<invalid-type {}>", offset.0)?,
                }
            }
            None => write!(w, "void")?,
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
            TypeKind::Unspecified(..) |
            TypeKind::PointerToMember(..) |
            TypeKind::Modifier(..) => false,
        }
    }

    fn is_subroutine(&self, hash: &FileHash) -> bool {
        match self.kind {
            TypeKind::Subroutine(..) => true,
            TypeKind::Def(ref val) => {
                match val.ty(hash) {
                    Some(ty) => ty.is_subroutine(hash),
                    None => false,
                }
            }
            TypeKind::Modifier(ref val) => {
                match val.ty(hash) {
                    Some(ty) => ty.is_subroutine(hash),
                    None => false,
                }
            }
            TypeKind::Struct(..) |
            TypeKind::Union(..) |
            TypeKind::Base(..) |
            TypeKind::Enumeration(..) |
            TypeKind::Array(..) |
            TypeKind::Unspecified(..) |
            TypeKind::PointerToMember(..) => false,
        }
    }

    /// Compare the identifying information of two types.
    /// This can be used to sort, and to determine if two types refer to the same definition
    /// (even if there are differences in the definitions).
    /// This must only be called for types that have identifiers.
    fn cmp_id(hash_a: &FileHash, type_a: &Type, hash_b: &FileHash, type_b: &Type) -> cmp::Ordering {
        use TypeKind::*;
        match (&type_a.kind, &type_b.kind) {
            (&Base(ref a), &Base(ref b)) => BaseType::cmp_id(a, b),
            (&Def(ref a), &Def(ref b)) => TypeDef::cmp_id(a, b),
            (&Struct(ref a), &Struct(ref b)) => StructType::cmp_id(a, b),
            (&Union(ref a), &Union(ref b)) => UnionType::cmp_id(a, b),
            (&Enumeration(ref a), &Enumeration(ref b)) => EnumerationType::cmp_id(a, b),
            (&Array(ref a), &Array(ref b)) => ArrayType::cmp_id(hash_a, a, hash_b, b),
            (&Subroutine(ref a), &Subroutine(ref b)) => {
                SubroutineType::cmp_id(hash_a, a, hash_b, b)
            }
            (&Unspecified(ref a), &Unspecified(ref b)) => UnspecifiedType::cmp_id(a, b),
            (&PointerToMember(ref a), &PointerToMember(ref b)) => {
                PointerToMemberType::cmp_id(hash_a, a, hash_b, b)
            }
            (&Modifier(ref a), &Modifier(ref b)) => TypeModifier::cmp_id(hash_a, a, hash_b, b),
            _ => {
                let discr_a = type_a.kind.discriminant_value();
                let discr_b = type_b.kind.discriminant_value();
                debug_assert_ne!(discr_a, discr_b);
                discr_a.cmp(&discr_b)
            }
        }
    }

    /// Compare the size of two types.
    fn cmp_size(
        hash_a: &FileHash,
        type_a: &Type,
        hash_b: &FileHash,
        type_b: &Type,
    ) -> cmp::Ordering {
        type_a.byte_size(hash_a).cmp(&type_b.byte_size(hash_b))
    }

    fn diff(
        w: &mut Write,
        state: &mut DiffState,
        unit_a: &Unit,
        type_a: &Type,
        unit_b: &Unit,
        type_b: &Type,
    ) -> Result<()> {
        use TypeKind::*;
        match (&type_a.kind, &type_b.kind) {
            (&Def(ref a), &Def(ref b)) => TypeDef::diff(w, state, unit_a, a, unit_b, b),
            (&Struct(ref a), &Struct(ref b)) => StructType::diff(w, state, unit_a, a, unit_b, b),
            (&Union(ref a), &Union(ref b)) => UnionType::diff(w, state, unit_a, a, unit_b, b),
            (&Enumeration(ref a), &Enumeration(ref b)) => EnumerationType::diff(w, state, a, b),
            _ => Err(format!("can't diff {:?}, {:?}", type_a, type_b).into()),
        }?;
        Ok(())
    }

    fn print_members(
        w: &mut Write,
        state: &mut PrintState,
        unit: &Unit,
        ty: Option<&Type>,
    ) -> Result<()> {
        if let Some(ty) = ty {
            match ty.kind {
                TypeKind::Struct(ref t) => return t.print_members(w, state, unit),
                TypeKind::Union(ref t) => return t.print_members(w, state, unit),
                _ => return Err(format!("can't print members {:?}", ty).into()),
            }
        }
        Ok(())
    }

    fn diff_members(
        w: &mut Write,
        state: &mut DiffState,
        unit_a: &Unit,
        type_a: Option<&Type>,
        unit_b: &Unit,
        type_b: Option<&Type>,
    ) -> Result<()> {
        if let (Some(type_a), Some(type_b)) = (type_a, type_b) {
            match (&type_a.kind, &type_b.kind) {
                (&TypeKind::Struct(ref a), &TypeKind::Struct(ref b)) => {
                    return StructType::diff_members(
                        w,
                        state,
                        unit_a,
                        a,
                        unit_b,
                        b,
                    );
                }
                (&TypeKind::Union(ref a), &TypeKind::Union(ref b)) => {
                    return UnionType::diff_members(
                        w,
                        state,
                        unit_a,
                        a,
                        unit_b,
                        b,
                    );
                }
                _ => {}
            }
        }

        state.prefix_diff(|state| {
            Type::print_members(w, &mut state.a, unit_a, type_a)?;
            Type::print_members(w, &mut state.b, unit_b, type_b)
        })
    }

    fn print_members_entries(
        w: &mut Write,
        state: &mut PrintState,
        unit: &Unit,
        ty: Option<&Type>,
    ) -> Result<()> {
        if let Some(ty) = ty {
            match ty.kind {
                TypeKind::Struct(ref t) => return t.print_members_entries(w, state, unit),
                TypeKind::Union(ref t) => return t.print_members_entries(w, state, unit),
                _ => return Err(format!("can't print members entries {:?}", ty).into()),
            }
        }
        Ok(())
    }

    fn diff_members_entries(
        w: &mut Write,
        state: &mut DiffState,
        unit_a: &Unit,
        type_a: Option<&Type>,
        unit_b: &Unit,
        type_b: Option<&Type>,
    ) -> Result<()> {
        if let (Some(type_a), Some(type_b)) = (type_a, type_b) {
            match (&type_a.kind, &type_b.kind) {
                (&TypeKind::Struct(ref a), &TypeKind::Struct(ref b)) => {
                    return StructType::diff_members_entries(
                        w,
                        state,
                        unit_a,
                        a,
                        unit_b,
                        b,
                    );
                }
                (&TypeKind::Union(ref a), &TypeKind::Union(ref b)) => {
                    return UnionType::diff_members_entries(
                        w,
                        state,
                        unit_a,
                        a,
                        unit_b,
                        b,
                    );
                }
                _ => {}
            }
        }

        state.prefix_diff(|state| {
            Type::print_members_entries(w, &mut state.a, unit_a, type_a)?;
            Type::print_members_entries(w, &mut state.b, unit_b, type_b)
        })
    }
}

#[derive(Debug)]
struct TypeModifier<'input> {
    kind: TypeModifierKind,
    ty: Option<TypeOffset>,
    name: Option<&'input [u8]>,
    byte_size: Option<u64>,
    // TODO: hack
    address_size: Option<u64>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum TypeModifierKind {
    Pointer,
    Reference,
    Const,
    Packed,
    Volatile,
    Restrict,
    Shared,
    RvalueReference,
    Atomic,
    // TODO:
    // Immutable,
    Other,
}

impl TypeModifierKind {
    fn discriminant_value(&self) -> u8 {
        match *self {
            TypeModifierKind::Pointer => 1,
            TypeModifierKind::Reference => 2,
            TypeModifierKind::Const => 3,
            TypeModifierKind::Packed => 4,
            TypeModifierKind::Volatile => 5,
            TypeModifierKind::Restrict => 6,
            TypeModifierKind::Shared => 7,
            TypeModifierKind::RvalueReference => 8,
            TypeModifierKind::Atomic => 9,
            TypeModifierKind::Other => 10,
        }
    }
}

impl<'input> TypeModifier<'input> {
    fn ty<'a>(&self, hash: &'a FileHash<'a, 'input>) -> Option<&'a Type<'input>>
        where 'input: 'a
    {
        self.ty.and_then(|v| Type::from_offset(hash, v))
    }

    fn byte_size(&self, hash: &FileHash) -> Option<u64> {
        if self.byte_size.is_some() {
            return self.byte_size;
        }
        match self.kind {
            TypeModifierKind::Const |
            TypeModifierKind::Packed |
            TypeModifierKind::Volatile |
            TypeModifierKind::Restrict |
            TypeModifierKind::Shared |
            TypeModifierKind::Atomic |
            TypeModifierKind::Other => self.ty(hash).and_then(|v| v.byte_size(hash)),
            TypeModifierKind::Pointer |
            TypeModifierKind::Reference |
            TypeModifierKind::RvalueReference => self.address_size,
        }
    }

    fn print_ref(&self, w: &mut Write, state: &PrintState) -> Result<()> {
        if let Some(name) = self.name {
            write!(w, "{}", String::from_utf8_lossy(name))?;
        } else {
            match self.kind {
                TypeModifierKind::Pointer => write!(w, "* ")?,
                TypeModifierKind::Reference |
                TypeModifierKind::RvalueReference => write!(w, "& ")?,
                TypeModifierKind::Const => write!(w, "const ")?,
                TypeModifierKind::Volatile => write!(w, "volatile ")?,
                TypeModifierKind::Restrict => write!(w, "restrict ")?,
                TypeModifierKind::Packed | TypeModifierKind::Shared |
                TypeModifierKind::Atomic | TypeModifierKind::Other => {}
            }
            Type::print_ref_from_offset(w, state, self.ty)?;
        }
        Ok(())
    }

    /// Compare the identifying information of two types.
    /// This can be used to sort, and to determine if two types refer to the same definition
    /// (even if there are differences in the definitions).
    fn cmp_id(
        hash_a: &FileHash,
        a: &TypeModifier,
        hash_b: &FileHash,
        b: &TypeModifier,
    ) -> cmp::Ordering {
        match (a.ty(hash_a), b.ty(hash_b)) {
            (Some(ty_a), Some(ty_b)) => {
                let ord = Type::cmp_id(hash_a, ty_a, hash_b, ty_b);
                if ord != cmp::Ordering::Equal {
                    return ord;
                }
            }
            (Some(_), None) => {
                return cmp::Ordering::Less;
            }
            (None, Some(_)) => {
                return cmp::Ordering::Greater;
            }
            (None, None) => {}
        }
        let discr_a = a.kind.discriminant_value();
        let discr_b = b.kind.discriminant_value();
        discr_a.cmp(&discr_b)
    }
}

#[derive(Debug, Default)]
struct BaseType<'input> {
    name: Option<&'input [u8]>,
    byte_size: Option<u64>,
}

impl<'input> BaseType<'input> {
    fn byte_size(&self) -> Option<u64> {
        self.byte_size
    }

    fn print_ref(&self, w: &mut Write) -> Result<()> {
        match self.name {
            Some(name) => write!(w, "{}", String::from_utf8_lossy(name))?,
            None => write!(w, "<anon-base-type>")?,
        }
        Ok(())
    }

    /// Compare the identifying information of two types.
    /// This can be used to sort, and to determine if two types refer to the same definition
    /// (even if there are differences in the definitions).
    fn cmp_id(a: &BaseType, b: &BaseType) -> cmp::Ordering {
        a.name.cmp(&b.name)
    }
}

#[derive(Debug, Default)]
struct TypeDef<'input> {
    namespace: Option<Rc<Namespace<'input>>>,
    name: Option<&'input [u8]>,
    ty: Option<TypeOffset>,
}

impl<'input> TypeDef<'input> {
    fn ty<'a>(&self, hash: &'a FileHash<'a, 'input>) -> Option<&'a Type<'input>>
        where 'input: 'a
    {
        self.ty.and_then(|t| Type::from_offset(hash, t))
    }

    fn byte_size(&self, hash: &FileHash) -> Option<u64> {
        self.ty(hash).and_then(|v| v.byte_size(hash))
    }

    fn print_ref(&self, w: &mut Write) -> Result<()> {
        if let Some(ref namespace) = self.namespace {
            namespace.print(w)?;
        }
        match self.name {
            Some(name) => write!(w, "{}", String::from_utf8_lossy(name))?,
            None => write!(w, "<anon-typedef>")?,
        }
        Ok(())
    }

    fn print_name(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        write!(w, "type ")?;
        self.print_ref(w)?;
        write!(w, " = ")?;
        Type::print_ref_from_offset(w, state, self.ty)?;
        Ok(())
    }

    fn print_byte_size(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        if let Some(byte_size) = self.byte_size(state.hash) {
            write!(w, "size: {}", byte_size)?;
        }
        Ok(())
    }

    fn filter(&self, flags: &Flags) -> bool {
        flags.filter_name(self.name) && flags.filter_namespace(&self.namespace)
    }

    fn print(&self, w: &mut Write, state: &mut PrintState, unit: &Unit) -> Result<()> {
        let ty = self.ty(state.hash);
        state.line(w, |w, state| self.print_name(w, state))?;
        state.indent(|state| {
            state.line(w, |w, state| self.print_byte_size(w, state))?;
            if let Some(ty) = ty {
                if ty.is_anon() {
                    Type::print_members(w, state, unit, Some(ty))?;
                }
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Compare the identifying information of two types.
    /// This can be used to sort, and to determine if two types refer to the same definition
    /// (even if there are differences in the definitions).
    fn cmp_id(a: &TypeDef, b: &TypeDef) -> cmp::Ordering {
        cmp_ns_and_name(&a.namespace, a.name, &b.namespace, b.name)
    }

    fn diff(
        w: &mut Write,
        state: &mut DiffState,
        unit_a: &Unit,
        a: &TypeDef,
        unit_b: &Unit,
        b: &TypeDef,
    ) -> Result<()> {
        state.line(w, a, b, |w, state, x| x.print_name(w, state))?;
        state.indent(|state| {
            state.line_option(w, a, b, |w, state, x| x.print_byte_size(w, state))?;
            let ty_a = filter_option(a.ty(state.a.hash), Type::is_anon);
            let ty_b = filter_option(b.ty(state.b.hash), Type::is_anon);
            Type::diff_members(w, state, unit_a, ty_a, unit_b, ty_b)
        })
    }
}

#[derive(Debug, Default)]
struct StructType<'input> {
    namespace: Option<Rc<Namespace<'input>>>,
    name: Option<&'input [u8]>,
    byte_size: Option<u64>,
    declaration: bool,
    members: Vec<Member<'input>>,
}

impl<'input> StructType<'input> {
    fn byte_size(&self) -> Option<u64> {
        self.byte_size
    }

    fn visit_members(&self, f: &mut FnMut(&Member) -> ()) {
        for member in &self.members {
            f(member);
        }
    }

    fn filter(&self, flags: &Flags) -> bool {
        flags.filter_name(self.name) && flags.filter_namespace(&self.namespace)
    }

    fn print(&self, w: &mut Write, state: &mut PrintState, unit: &Unit) -> Result<()> {
        state.line(w, |w, _state| self.print_ref(w))?;
        state.indent(|state| {
            state.line_option(w, |w, state| self.print_declaration(w, state))?;
            state.line_option(w, |w, state| self.print_byte_size(w, state))?;
            self.print_members(w, state, unit)
        })
    }

    fn diff(
        w: &mut Write,
        state: &mut DiffState,
        unit_a: &Unit,
        a: &StructType,
        unit_b: &Unit,
        b: &StructType,
    ) -> Result<()> {
        // The names should be the same, but we can't be sure.
        state.line(w, a, b, |w, _state, x| x.print_ref(w))?;
        state.indent(|state| {
            state.line_option(w, a, b, |w, state, x| x.print_declaration(w, state))?;
            state.line_option(w, a, b, |w, state, x| x.print_byte_size(w, state))?;
            Self::diff_members(w, state, unit_a, a, unit_b, b)
        })?;

        Ok(())
    }

    fn print_members(
        &self,
        w: &mut Write,
        state: &mut PrintState,
        unit: &Unit,
    ) -> Result<()> {
        state.line_option(w, |w, state| self.print_members_label(w, state))?;
        state.indent(|state| self.print_members_entries(w, state, unit))
    }

    fn diff_members(
        w: &mut Write,
        state: &mut DiffState,
        unit_a: &Unit,
        a: &StructType,
        unit_b: &Unit,
        b: &StructType,
    ) -> Result<()> {
        state.line_option(w, a, b, |w, state, x| x.print_members_label(w, state))?;
        state.indent(|state| {
            Self::diff_members_entries(w, state, unit_a, a, unit_b, b)
        })
    }

    fn print_byte_size(&self, w: &mut Write, _state: &mut PrintState) -> Result<()> {
        if let Some(size) = self.byte_size {
            write!(w, "size: {}", size)?;
        } else if !self.declaration {
            debug!("struct with no size");
        }
        Ok(())
    }

    fn print_declaration(&self, w: &mut Write, _state: &mut PrintState) -> Result<()> {
        if self.declaration {
            write!(w, "declaration: yes")?;
        }
        Ok(())
    }

    fn print_members_label(&self, w: &mut Write, _state: &mut PrintState) -> Result<()> {
        if !self.members.is_empty() {
            write!(w, "members:")?;
        }
        Ok(())
    }

    fn print_members_entries(
        &self,
        w: &mut Write,
        state: &mut PrintState,
        unit: &Unit,
    ) -> Result<()> {
        let mut bit_offset = Some(0);
        for member in &self.members {
            state.line_option(
                w,
                |w, state| Member::print_padding(w, state, member.padding(bit_offset)),
            )?;
            member.print(w, state, unit, &mut bit_offset)?;
        }
        state.line_option(w, |w, state| Member::print_padding(w, state, self.padding(bit_offset)))?;
        Ok(())
    }

    fn diff_members_entries(
        w: &mut Write,
        state: &mut DiffState,
        unit_a: &Unit,
        a: &StructType,
        unit_b: &Unit,
        b: &StructType,
    ) -> Result<()> {
        let mut bit_offset_a = Some(0);
        let mut bit_offset_b = Some(0);
        state.list(w,
                   &a.members,
                   unit_a,
                   &mut bit_offset_a,
                   &b.members,
                   unit_b,
                   &mut bit_offset_b,
                   Member::step_cost(),
                   Member::diff_cost,
        |w, state, x, unit, bit_offset| x.print(w, state, unit, bit_offset),
        |w, state, a, unit_a, bit_offset_a, b, unit_b, bit_offset_b| {
            state.line_option(
                w,
                a.padding(*bit_offset_a),
                b.padding(*bit_offset_b),
                Member::print_padding
                )?;
            Member::diff(
                w,
                state,
                unit_a,
                a,
                bit_offset_a,
                unit_b,
                b,
                bit_offset_b,
                )
        })?;

        state.line_option(
            w,
            a.padding(bit_offset_a),
            b.padding(bit_offset_b),
            Member::print_padding,
        )?;
        Ok(())
    }

    // Returns (offset, size) of padding.
    fn padding(&self, bit_offset: Option<u64>) -> Option<(u64, u64)> {
        if let (Some(bit_offset), Some(size)) = (bit_offset, self.byte_size) {
            if bit_offset < size * 8 {
                return Some((bit_offset, size * 8 - bit_offset));
            }
        }
        None
    }

    fn print_ref(&self, w: &mut Write) -> Result<()> {
        write!(w, "struct ")?;
        if let Some(ref namespace) = self.namespace {
            namespace.print(w)?;
        }
        match self.name {
            Some(name) => write!(w, "{}", String::from_utf8_lossy(name))?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn is_anon(&self) -> bool {
        self.name.is_none() || Namespace::is_anon_type(&self.namespace)
    }

    /// Compare the identifying information of two types.
    /// This can be used to sort, and to determine if two types refer to the same definition
    /// (even if there are differences in the definitions).
    fn cmp_id(a: &StructType, b: &StructType) -> cmp::Ordering {
        cmp_ns_and_name(&a.namespace, a.name, &b.namespace, b.name)
    }
}

#[derive(Debug, Default)]
struct UnionType<'input> {
    namespace: Option<Rc<Namespace<'input>>>,
    name: Option<&'input [u8]>,
    byte_size: Option<u64>,
    declaration: bool,
    members: Vec<Member<'input>>,
}

impl<'input> UnionType<'input> {
    fn byte_size(&self) -> Option<u64> {
        self.byte_size
    }

    fn visit_members(&self, f: &mut FnMut(&Member) -> ()) {
        for member in &self.members {
            f(member);
        }
    }

    fn filter(&self, flags: &Flags) -> bool {
        flags.filter_name(self.name) && flags.filter_namespace(&self.namespace)
    }

    fn print(&self, w: &mut Write, state: &mut PrintState, unit: &Unit) -> Result<()> {
        state.line(w, |w, _state| self.print_ref(w))?;
        state.indent(|state| {
            state.line_option(w, |w, state| self.print_declaration(w, state))?;
            state.line_option(w, |w, state| self.print_byte_size(w, state))?;
            self.print_members(w, state, unit)
        })
    }

    fn diff(
        w: &mut Write,
        state: &mut DiffState,
        unit_a: &Unit,
        a: &UnionType,
        unit_b: &Unit,
        b: &UnionType,
    ) -> Result<()> {
        // The names should be the same, but we can't be sure.
        state.line(w, a, b, |w, _state, x| x.print_ref(w))?;
        state.indent(|state| {
            state.line_option(w, a, b, |w, state, x| x.print_declaration(w, state))?;
            state.line_option(w, a, b, |w, state, x| x.print_byte_size(w, state))?;
            Self::diff_members(w, state, unit_a, a, unit_b, b)
        })?;

        Ok(())
    }

    fn print_members(
        &self,
        w: &mut Write,
        state: &mut PrintState,
        unit: &Unit,
    ) -> Result<()> {
        state.line_option(w, |w, state| self.print_members_label(w, state))?;
        state.indent(|state| self.print_members_entries(w, state, unit))
    }

    fn diff_members(
        w: &mut Write,
        state: &mut DiffState,
        unit_a: &Unit,
        a: &UnionType,
        unit_b: &Unit,
        b: &UnionType,
    ) -> Result<()> {
        state.line_option(w, a, b, |w, state, x| x.print_members_label(w, state))?;
        state.indent(|state| {
            Self::diff_members_entries(w, state, unit_a, a, unit_b, b)
        })
    }

    fn print_byte_size(&self, w: &mut Write, _state: &mut PrintState) -> Result<()> {
        if let Some(size) = self.byte_size {
            write!(w, "size: {}", size)?;
        } else if !self.declaration {
            debug!("struct with no size");
        }
        Ok(())
    }

    fn print_declaration(&self, w: &mut Write, _state: &mut PrintState) -> Result<()> {
        if self.declaration {
            write!(w, "declaration: yes")?;
        }
        Ok(())
    }

    fn print_members_label(&self, w: &mut Write, _state: &mut PrintState) -> Result<()> {
        if !self.members.is_empty() {
            write!(w, "members:")?;
        }
        Ok(())
    }

    fn print_members_entries(
        &self,
        w: &mut Write,
        state: &mut PrintState,
        unit: &Unit,
    ) -> Result<()> {
        for member in &self.members {
            // TODO: padding?
            member.print(w, state, unit, &mut None)?;
        }
        // TODO: trailing padding?
        Ok(())
    }

    fn diff_members_entries(
        w: &mut Write,
        state: &mut DiffState,
        unit_a: &Unit,
        a: &UnionType,
        unit_b: &Unit,
        b: &UnionType,
    ) -> Result<()> {
        // TODO: handle reordering better
        state.list(w,
                   &a.members,
                   unit_a,
                   &mut (),
                   &b.members,
                   unit_b,
                   &mut (),
                   Member::step_cost(),
                   Member::diff_cost,
        |w, state, x, unit, _| x.print(w, state, unit, &mut None),
        |w, state, a, unit_a, _, b, unit_b, _| {
            // TODO: padding?
            Member::diff(
                w,
                state,
                unit_a,
                a,
                &mut None,
                unit_b,
                b,
                &mut None,
                )
        })?;

        // TODO: trailing padding?
        Ok(())
    }

    fn print_ref(&self, w: &mut Write) -> Result<()> {
        write!(w, "union ")?;
        if let Some(ref namespace) = self.namespace {
            namespace.print(w)?;
        }
        match self.name {
            Some(name) => write!(w, "{}", String::from_utf8_lossy(name))?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn is_anon(&self) -> bool {
        self.name.is_none() || Namespace::is_anon_type(&self.namespace)
    }

    /// Compare the identifying information of two types.
    /// This can be used to sort, and to determine if two types refer to the same definition
    /// (even if there are differences in the definitions).
    fn cmp_id(a: &UnionType, b: &UnionType) -> cmp::Ordering {
        cmp_ns_and_name(&a.namespace, a.name, &b.namespace, b.name)
    }
}

#[derive(Debug, Default, Clone)]
struct Member<'input> {
    name: Option<&'input [u8]>,
    // TODO: treat padding as typeless member?
    ty: Option<TypeOffset>,
    // Defaults to 0, so always present.
    bit_offset: u64,
    bit_size: Option<u64>,
}

impl<'input> Member<'input> {
    fn ty<'a>(&self, hash: &'a FileHash<'a, 'input>) -> Option<&'a Type<'input>>
        where 'input: 'a
    {
        self.ty.and_then(|t| Type::from_offset(hash, t))
    }

    fn bit_size(&self, hash: &FileHash) -> Option<u64> {
        if self.bit_size.is_some() {
            self.bit_size
        } else {
            self.ty(hash).and_then(|v| v.byte_size(hash).map(|v| v * 8))
        }
    }

    fn print(
        &self,
        w: &mut Write,
        state: &mut PrintState,
        unit: &Unit,
        end_bit_offset: &mut Option<u64>,
    ) -> Result<()> {
        state.line(w, |w, state| self.print_name(w, state, end_bit_offset))?;
        state.indent(|state| {
            let ty = if self.is_inline(state.hash) {
                self.ty(state.hash)
            } else {
                None
            };
            Type::print_members_entries(w, state, unit, ty)
        })
    }

    fn diff(
        w: &mut Write,
        state: &mut DiffState,
        unit_a: &Unit,
        a: &Member,
        end_bit_offset_a: &mut Option<u64>,
        unit_b: &Unit,
        b: &Member,
        end_bit_offset_b: &mut Option<u64>,
    ) -> Result<()> {
        state.line(
            w,
            (a, end_bit_offset_a),
            (b, end_bit_offset_b),
            |w, state, (x, end_bit_offset)| x.print_name(w, state, end_bit_offset),
        )?;
        state.indent(|state| {
            let ty_a = if a.is_inline(state.a.hash) {
                a.ty(state.a.hash)
            } else {
                None
            };
            let ty_b = if b.is_inline(state.b.hash) {
                b.ty(state.b.hash)
            } else {
                None
            };
            Type::diff_members_entries(
                w,
                state,
                unit_a,
                ty_a,
                unit_b,
                ty_b,
            )
        })
    }

    fn step_cost() -> usize {
        1
    }

    fn diff_cost(state: &DiffState, a: &Member, b: &Member) -> usize {
        let mut cost = 0;
        if a.name.cmp(&b.name) != cmp::Ordering::Equal {
            cost += 1;
        }
        match (a.ty(state.a.hash), b.ty(state.b.hash)) {
            (Some(ty_a), Some(ty_b)) => {
                if Type::cmp_id(state.a.hash, ty_a, state.b.hash, ty_b) != cmp::Ordering::Equal {
                    cost += 1;
                }
            }
            (None, None) => {}
            _ => {
                cost += 1;
            }
        }
        cost
    }

    // Returns (offset, size) of padding.
    fn padding(&self, end_bit_offset: Option<u64>) -> Option<(u64, u64)> {
        if let Some(end_bit_offset) = end_bit_offset {
            if self.bit_offset > end_bit_offset {
                return Some((end_bit_offset, self.bit_offset - end_bit_offset));
            }
        }
        None
    }

    fn print_padding(
        w: &mut Write,
        _state: &mut PrintState,
        padding: Option<(u64, u64)>,
    ) -> Result<()> {
        if let Some((padding_bit_offset, padding_bit_size)) = padding {
            write!(
                w,
                "{}[{}]\t<padding>",
                format_bit(padding_bit_offset),
                format_bit(padding_bit_size)
            )?;
        }
        Ok(())
    }

    fn print_name(
        &self,
        w: &mut Write,
        state: &mut PrintState,
        end_bit_offset: &mut Option<u64>,
    ) -> Result<()> {
        write!(w, "{}", format_bit(self.bit_offset))?;
        match self.bit_size(state.hash) {
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
            Some(name) => write!(w, "\t{}", String::from_utf8_lossy(name))?,
            None => write!(w, "\t<anon>")?,
        }
        write!(w, ": ")?;
        Type::print_ref_from_offset(w, state, self.ty)?;
        Ok(())
    }

    fn is_inline(&self, hash: &FileHash) -> bool {
        match self.name {
            Some(s) => {
                if s.starts_with(b"RUST$ENCODED$ENUM$") {
                    return true;
                }
            }
            None => return true,
        };
        if let Some(ty) = self.ty(hash) {
            ty.is_anon()
        } else {
            false
        }
    }
}

#[derive(Debug, Default)]
struct EnumerationType<'input> {
    namespace: Option<Rc<Namespace<'input>>>,
    name: Option<&'input [u8]>,
    declaration: bool,
    ty: Option<TypeOffset>,
    byte_size: Option<u64>,
    enumerators: Vec<Enumerator<'input>>,
}

impl<'input> EnumerationType<'input> {
    fn ty<'a>(&self, hash: &'a FileHash<'a, 'input>) -> Option<&'a Type<'input>>
        where 'input: 'a
    {
        self.ty.and_then(|t| Type::from_offset(hash, t))
    }

    fn byte_size(&self, hash: &FileHash) -> Option<u64> {
        if self.byte_size.is_some() {
            self.byte_size
        } else {
            self.ty(hash).and_then(|v| v.byte_size(hash))
        }
    }

    fn filter(&self, flags: &Flags) -> bool {
        flags.filter_name(self.name) && flags.filter_namespace(&self.namespace)
    }

    /// Compare the identifying information of two types.
    /// This can be used to sort, and to determine if two types refer to the same definition
    /// (even if there are differences in the definitions).
    fn cmp_id(a: &EnumerationType, b: &EnumerationType) -> cmp::Ordering {
        cmp_ns_and_name(&a.namespace, a.name, &b.namespace, b.name)
    }

    fn print(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        state.line(w, |w, _state| self.print_ref(w))?;
        state.indent(|state| {
            state.line_option(w, |w, _state| self.print_declaration(w))?;
            state.line_option(w, |w, state| self.print_byte_size(w, state))?;
            self.print_enumerators(w, state)
        })
    }

    fn diff(
        w: &mut Write,
        state: &mut DiffState,
        a: &EnumerationType,
        b: &EnumerationType,
    ) -> Result<()> {
        // The names should be the same, but we can't be sure.
        state.line(w, a, b, |w, _state, x| x.print_ref(w))?;
        state.indent(|state| {
            state.line_option(w, a, b, |w, _state, x| x.print_declaration(w))?;
            state.line_option(w, a, b, |w, state, x| x.print_byte_size(w, state))?;
            Self::diff_enumerators(w, state, a, b)
        })?;
        Ok(())
    }

    fn print_ref(&self, w: &mut Write) -> Result<()> {
        write!(w, "enum ")?;
        if let Some(ref namespace) = self.namespace {
            namespace.print(w)?;
        }
        match self.name {
            Some(name) => write!(w, "{}", String::from_utf8_lossy(name))?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn print_declaration(&self, w: &mut Write) -> Result<()> {
        if self.declaration {
            write!(w, "declaration: yes")?;
        }
        Ok(())
    }

    fn print_byte_size(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        if let Some(size) = self.byte_size(state.hash) {
            write!(w, "size: {}", size)?;
        } else {
            debug!("enum with no size");
        }
        Ok(())
    }

    fn print_enumerators(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        state.line_option(w, |w, state| self.print_enumerators_label(w, state))?;
        state.indent(|state| self.print_enumerators_entries(w, state))
    }

    fn diff_enumerators(
        w: &mut Write,
        state: &mut DiffState,
        a: &EnumerationType,
        b: &EnumerationType,
    ) -> Result<()> {
        state.line_option(w, a, b, |w, state, x| x.print_enumerators_label(w, state))?;
        state.indent(|state| Self::diff_enumerators_entries(w, state, a, b))
    }

    fn print_enumerators_label(&self, w: &mut Write, _state: &mut PrintState) -> Result<()> {
        if !self.enumerators.is_empty() {
            write!(w, "enumerators:")?;
        } else {
            debug!("enum with no enumerators");
        }
        Ok(())
    }

    fn print_enumerators_entries(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        for enumerator in &self.enumerators {
            enumerator.print(w, state)?;
        }
        Ok(())
    }

    fn diff_enumerators_entries(
        w: &mut Write,
        state: &mut DiffState,
        a: &EnumerationType,
        b: &EnumerationType,
    ) -> Result<()> {
        // TODO: handle reordering better
        state.list(w,
                   &a.enumerators,
                   (),
                   &mut (),
                   &b.enumerators,
                   (),
                   &mut (),
                   Enumerator::step_cost(),
                   Enumerator::diff_cost,
        |w, state, x, _, _| x.print(w, state),
        |w, state, a, _, _, b, _, _| {
            Enumerator::diff(
                w,
                state,
                a,
                b,
                )
        })?;
        Ok(())
    }
}

#[derive(Debug, Default, Clone)]
struct Enumerator<'input> {
    name: Option<&'input [u8]>,
    value: Option<i64>,
}

impl<'input> Enumerator<'input> {
    fn print_ref(&self, w: &mut Write) -> Result<()> {
        match self.name {
            Some(name) => write!(w, "{}", String::from_utf8_lossy(name))?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn print(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        state.line(w, |w, _state| self.print_name_value(w))
    }

    fn diff(w: &mut Write, state: &mut DiffState, a: &Enumerator, b: &Enumerator) -> Result<()> {
        state.line(w, a, b, |w, _state, x| x.print_name_value(w))
    }

    fn step_cost() -> usize {
        3
    }

    fn diff_cost(_state: &DiffState, a: &Enumerator, b: &Enumerator) -> usize {
        // A difference in name is usually more significant than a difference in value,
        // such as for enums where the value is assigned by the compiler.
        let mut cost = 0;
        if a.name.cmp(&b.name) != cmp::Ordering::Equal {
            cost += 4;
        }
        if a.value.cmp(&b.value) != cmp::Ordering::Equal {
            cost += 2;
        }
        cost
    }

    fn print_name_value(&self, w: &mut Write) -> Result<()> {
        self.print_ref(w)?;
        if let Some(value) = self.value {
            write!(w, "({})", value)?;
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct ArrayType<'input> {
    ty: Option<TypeOffset>,
    count: Option<u64>,
    byte_size: Option<u64>,
    phantom: std::marker::PhantomData<&'input [u8]>,
}

impl<'input> ArrayType<'input> {
    fn ty<'a>(&self, hash: &'a FileHash<'a, 'input>) -> Option<&'a Type<'input>>
        where 'input: 'a
    {
        self.ty.and_then(|v| Type::from_offset(hash, v))
    }

    fn byte_size(&self, hash: &FileHash) -> Option<u64> {
        if self.byte_size.is_some() {
            self.byte_size
        } else if let (Some(ty), Some(count)) = (self.ty(hash), self.count) {
            ty.byte_size(hash).map(|v| v * count)
        } else {
            None
        }
    }

    fn count(&self, hash: &FileHash) -> Option<u64> {
        if self.count.is_some() {
            self.count
        } else if let (Some(ty), Some(byte_size)) = (self.ty(hash), self.byte_size) {
            ty.byte_size(hash).map(|v| byte_size / v)
        } else {
            None
        }
    }

    fn print_ref(&self, w: &mut Write, state: &PrintState) -> Result<()> {
        write!(w, "[")?;
        Type::print_ref_from_offset(w, state, self.ty)?;
        if let Some(count) = self.count(state.hash) {
            write!(w, "; {}", count)?;
        }
        write!(w, "]")?;
        Ok(())
    }

    /// Compare the identifying information of two types.
    /// This can be used to sort, and to determine if two types refer to the same definition
    /// (even if there are differences in the definitions).
    fn cmp_id(hash_a: &FileHash, a: &ArrayType, hash_b: &FileHash, b: &ArrayType) -> cmp::Ordering {
        match (a.ty(hash_a), b.ty(hash_b)) {
            (Some(ty_a), Some(ty_b)) => {
                let ord = Type::cmp_id(hash_a, ty_a, hash_b, ty_b);
                if ord != cmp::Ordering::Equal {
                    return ord;
                }
            }
            (Some(_), None) => {
                return cmp::Ordering::Less;
            }
            (None, Some(_)) => {
                return cmp::Ordering::Greater;
            }
            (None, None) => {}
        }
        a.count.cmp(&b.count)
    }
}

#[derive(Debug, Default)]
struct SubroutineType<'input> {
    parameters: Vec<Parameter<'input>>,
    return_type: Option<TypeOffset>,
    byte_size: Option<u64>,
}

impl<'input> SubroutineType<'input> {
    fn byte_size(&self) -> Option<u64> {
        self.byte_size
    }

    fn return_type<'a>(&self, hash: &'a FileHash<'a, 'input>) -> Option<&'a Type<'input>>
        where 'input: 'a
    {
        self.return_type.and_then(|v| Type::from_offset(hash, v))
    }

    fn print_ref(&self, w: &mut Write, state: &PrintState) -> Result<()> {
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

        if let Some(return_type) = self.return_type(state.hash) {
            write!(w, " -> ")?;
            return_type.print_ref(w, state)?;
        }
        Ok(())
    }

    /// Compare the identifying information of two types.
    /// This can be used to sort, and to determine if two types refer to the same definition
    /// (even if there are differences in the definitions).
    fn cmp_id(
        hash_a: &FileHash,
        a: &SubroutineType,
        hash_b: &FileHash,
        b: &SubroutineType,
    ) -> cmp::Ordering {
        for (parameter_a, parameter_b) in a.parameters.iter().zip(b.parameters.iter()) {
            let ord = Parameter::cmp_id(hash_a, parameter_a, hash_b, parameter_b);
            if ord != cmp::Ordering::Equal {
                return ord;
            }
        }

        let ord = a.parameters.len().cmp(&b.parameters.len());
        if ord != cmp::Ordering::Equal {
            return ord;
        }

        match (a.return_type(hash_a), b.return_type(hash_b)) {
            (Some(ty_a), Some(ty_b)) => {
                let ord = Type::cmp_id(hash_a, ty_a, hash_b, ty_b);
                if ord != cmp::Ordering::Equal {
                    return ord;
                }
            }
            (Some(_), None) => {
                return cmp::Ordering::Less;
            }
            (None, Some(_)) => {
                return cmp::Ordering::Greater;
            }
            (None, None) => {}
        }

        cmp::Ordering::Equal
    }
}

#[derive(Debug, Default)]
struct UnspecifiedType<'input> {
    namespace: Option<Rc<Namespace<'input>>>,
    name: Option<&'input [u8]>,
}

impl<'input> UnspecifiedType<'input> {
    fn filter(&self, flags: &Flags) -> bool {
        flags.filter_name(self.name) && flags.filter_namespace(&self.namespace)
    }

    fn print_ref(&self, w: &mut Write) -> Result<()> {
        if let Some(ref namespace) = self.namespace {
            namespace.print(w)?;
        }
        match self.name {
            Some(name) => write!(w, "{}", String::from_utf8_lossy(name))?,
            None => write!(w, "<void>")?,
        }
        Ok(())
    }

    /// Compare the identifying information of two types.
    /// This can be used to sort, and to determine if two types refer to the same definition
    /// (even if there are differences in the definitions).
    fn cmp_id(a: &UnspecifiedType, b: &UnspecifiedType) -> cmp::Ordering {
        cmp_ns_and_name(&a.namespace, a.name, &b.namespace, b.name)
    }
}

#[derive(Debug, Default)]
struct PointerToMemberType {
    ty: Option<TypeOffset>,
    containing_ty: Option<TypeOffset>,
    byte_size: Option<u64>,
    // TODO: hack
    address_size: Option<u64>,
}

impl PointerToMemberType {
    fn ty<'a, 'input>(&self, hash: &'a FileHash<'a, 'input>) -> Option<&'a Type<'input>>
        where 'input: 'a
    {
        self.ty.and_then(|v| Type::from_offset(hash, v))
    }

    fn containing_ty<'a, 'input>(&self, hash: &'a FileHash<'a, 'input>) -> Option<&'a Type<'input>>
        where 'input: 'a
    {
        self.containing_ty.and_then(|v| Type::from_offset(hash, v))
    }

    fn byte_size(&self, hash: &FileHash) -> Option<u64> {
        if self.byte_size.is_some() {
            return self.byte_size;
        }
        // TODO: this probably depends on the ABI
        self.ty(hash).and_then(|ty| if ty.is_subroutine(hash) {
            self.address_size.map(|v| v * 2)
        } else {
            self.address_size
        })
    }

    fn print_ref(&self, w: &mut Write, state: &PrintState) -> Result<()> {
        Type::print_ref_from_offset(w, state, self.containing_ty)?;
        write!(w, "::* ")?;
        Type::print_ref_from_offset(w, state, self.ty)?;
        Ok(())
    }

    /// Compare the identifying information of two types.
    /// This can be used to sort, and to determine if two types refer to the same definition
    /// (even if there are differences in the definitions).
    fn cmp_id(
        hash_a: &FileHash,
        a: &PointerToMemberType,
        hash_b: &FileHash,
        b: &PointerToMemberType,
    ) -> cmp::Ordering {
        match (a.containing_ty(hash_a), b.containing_ty(hash_b)) {
            (Some(ty_a), Some(ty_b)) => {
                let ord = Type::cmp_id(hash_a, ty_a, hash_b, ty_b);
                if ord != cmp::Ordering::Equal {
                    return ord;
                }
            }
            (Some(_), None) => {
                return cmp::Ordering::Less;
            }
            (None, Some(_)) => {
                return cmp::Ordering::Greater;
            }
            (None, None) => {}
        }
        match (a.ty(hash_a), b.ty(hash_b)) {
            (Some(ty_a), Some(ty_b)) => {
                let ord = Type::cmp_id(hash_a, ty_a, hash_b, ty_b);
                if ord != cmp::Ordering::Equal {
                    return ord;
                }
            }
            (Some(_), None) => {
                return cmp::Ordering::Less;
            }
            (None, Some(_)) => {
                return cmp::Ordering::Greater;
            }
            (None, None) => {}
        }
        cmp::Ordering::Equal
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SubprogramOffset(usize);

impl From<gimli::DebugInfoOffset> for SubprogramOffset {
    fn from(o: gimli::DebugInfoOffset) -> SubprogramOffset {
        SubprogramOffset(o.0)
    }
}

#[derive(Debug)]
struct Subprogram<'input> {
    namespace: Option<Rc<Namespace<'input>>>,
    name: Option<&'input [u8]>,
    linkage_name: Option<&'input [u8]>,
    low_pc: Option<u64>,
    high_pc: Option<u64>,
    size: Option<u64>,
    inline: bool,
    declaration: bool,
    parameters: Vec<Parameter<'input>>,
    return_type: Option<TypeOffset>,
    inlined_subroutines: Vec<InlinedSubroutine<'input>>,
    variables: Vec<Variable<'input>>,
}

impl<'input> Subprogram<'input> {
    fn from_offset<'a>(
        unit: &'a Unit<'input>,
        offset: SubprogramOffset,
    ) -> Option<&'a Subprogram<'input>> {
        unit.subprograms.get(&offset)
    }

    fn filter(&self, flags: &Flags) -> bool {
        flags.filter_name(self.name) && flags.filter_namespace(&self.namespace)
    }

    fn calls(&self, file: &File) -> Vec<u64> {
        if let (Some(low_pc), Some(high_pc)) = (self.low_pc, self.high_pc) {
            if low_pc != 0 {
                if let Some(ref code) = file.code {
                    return disassemble(code, low_pc, high_pc);
                }
            }
        }
        Vec::new()
    }

    /// Compare the identifying information of two subprograms.
    /// This can be used to sort, and to determine if two subprograms refer to the same definition
    /// (even if there are differences in the definitions).
    fn cmp_id(a: &Subprogram, b: &Subprogram) -> cmp::Ordering {
        cmp_ns_and_name(&a.namespace, a.name, &b.namespace, b.name)
    }

    /// Compare the size of two subprograms.
    fn cmp_size(a: &Subprogram, b: &Subprogram) -> cmp::Ordering {
        a.size.cmp(&b.size)
    }

    fn print_ref(&self, w: &mut Write) -> Result<()> {
        if let Some(ref namespace) = self.namespace {
            namespace.print(w)?;
        }
        match self.name {
            Some(name) => write!(w, "{}", String::from_utf8_lossy(name))?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn print(&self, w: &mut Write, state: &mut PrintState, unit: &Unit) -> Result<()> {
        state.line(w, |w, _state| self.print_name(w))?;
        state.indent(|state| {
            state.line_option(w, |w, _state| self.print_linkage_name(w))?;
            state.line_option(w, |w, _state| self.print_address(w))?;
            state.line_option(w, |w, _state| self.print_size(w))?;
            state.line_option(w, |w, _state| self.print_inline(w))?;
            state.line_option(w, |w, _state| self.print_declaration(w))?;
            state.line_option(w, |w, _state| self.print_return_type_label(w))?;
            state
                    .indent(
                        |state| state.line_option(w, |w, state| self.print_return_type(w, state)),
                    )?;
            state.line_option(w, |w, _state| self.print_parameters_label(w))?;
            state.indent(|state| self.print_parameters(w, state))?;
            state.line_option(w, |w, _state| self.print_variables_label(w))?;
            state.indent(|state| self.print_variables(w, state))?;
            if state.flags.inline_depth > 0 {
                state.line_option(w, |w, _state| self.print_inlined_subroutines_label(w))?;
                state.indent(|state| self.print_inlined_subroutines(w, state, unit))?;
            }
            if state.flags.calls {
                let calls = self.calls(state.file);
                if !calls.is_empty() {
                    state.line(w, |w, _state| self.print_calls_label(w))?;
                    state.indent(|state| self.print_calls(w, state, &calls))?;
                }
            }
            Ok(())
        })
    }

    fn diff(
        w: &mut Write,
        state: &mut DiffState,
        unit_a: &Unit,
        a: &Subprogram,
        unit_b: &Unit,
        b: &Subprogram,
    ) -> Result<()> {
        state.line(w, a, b, |w, _state, x| x.print_name(w))?;
        state.indent(|state| {
            state.line_option(w, a, b, |w, _state, x| x.print_linkage_name(w))?;
            let flag = state.flags.ignore_function_address;
            state.ignore_diff(
                flag,
                |state| state.line_option(w, a, b, |w, _state, x| x.print_address(w)),
            )?;
            let flag = state.flags.ignore_function_size;
            state.ignore_diff(
                flag,
                |state| state.line_option(w, a, b, |w, _state, x| x.print_size(w)),
            )?;
            let flag = state.flags.ignore_function_inline;
            state.ignore_diff(
                flag,
                |state| state.line_option(w, a, b, |w, _state, x| x.print_inline(w)),
            )?;
            state.line_option(w, a, b, |w, _state, x| x.print_declaration(w))?;
            state.line_option(w, a, b, |w, _state, x| x.print_return_type_label(w))?;
            state.indent(
                |state| state.line_option(w, a, b, |w, state, x| x.print_return_type(w, state)),
            )?;
            state.line_option(w, a, b, |w, _state, x| x.print_parameters_label(w))?;
            state.indent(|state| Subprogram::diff_parameters(w, state, a, b))?;
            state.line_option(w, a, b, |w, _state, x| x.print_variables_label(w))?;
            state.indent(|state| Subprogram::diff_variables(w, state, a, b))?;
            if state.flags.inline_depth > 0 {
                state.line_option(w, a, b, |w, _state, x| x.print_inlined_subroutines_label(w))?;
                InlinedSubroutine::diff_inlined_subroutines(w, state, unit_a, &a.inlined_subroutines, unit_b, &b.inlined_subroutines, 1)?;
            }
            // TODO
            if false && state.flags.calls {
                let calls_a = a.calls(state.a.file);
                let calls_b = b.calls(state.b.file);
                state.line_option(w, (a, &calls_a), (b, &calls_b), |w, _state, (x, calls)| {
                    if !calls.is_empty() {
                        x.print_calls_label(w)?;
                    }
                    Ok(())
                })?;
                state.indent(|state| Subprogram::diff_calls(w, state, &calls_a, &calls_b))?;
            }
            Ok(())
        })
    }

    fn print_name(&self, w: &mut Write) -> Result<()> {
        write!(w, "fn ")?;
        if let Some(ref namespace) = self.namespace {
            namespace.print(w)?;
        }
        match self.name {
            Some(name) => write!(w, "{}", String::from_utf8_lossy(name))?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn print_linkage_name(&self, w: &mut Write) -> Result<()> {
        if let Some(linkage_name) = self.linkage_name {
            write!(w, "linkage name: {}", String::from_utf8_lossy(linkage_name))?;
        }
        Ok(())
    }

    fn print_address(&self, w: &mut Write) -> Result<()> {
        if let (Some(low_pc), Some(high_pc)) = (self.low_pc, self.high_pc) {
            if high_pc > low_pc {
                write!(w, "address: 0x{:x}-0x{:x}", low_pc, high_pc - 1)?;
            } else {
                write!(w, "address: 0x{:x}", low_pc)?;
            }
        } else if let Some(low_pc) = self.low_pc {
            write!(w, "address: 0x{:x}", low_pc)?;
        } else if !self.inline && !self.declaration {
            debug!("non-inline subprogram with no address");
        }
        Ok(())
    }

    fn print_size(&self, w: &mut Write) -> Result<()> {
        if let Some(size) = self.size {
            write!(w, "size: {}", size)?;
        }
        Ok(())
    }

    fn print_inline(&self, w: &mut Write) -> Result<()> {
        if self.inline {
            write!(w, "inline: yes")?;
        }
        Ok(())
    }

    fn print_declaration(&self, w: &mut Write) -> Result<()> {
        if self.declaration {
            write!(w, "declaration: yes")?;
        }
        Ok(())
    }

    fn print_return_type_label(&self, w: &mut Write) -> Result<()> {
        if self.return_type.is_some() {
            write!(w, "return type:")?;
        }
        Ok(())
    }

    fn print_return_type(&self, w: &mut Write, state: &PrintState) -> Result<()> {
        if let Some(return_type) = self.return_type {
            match Type::from_offset(state.hash, return_type).and_then(|t| t.byte_size(state.hash)) {
                Some(byte_size) => write!(w, "[{}]", byte_size)?,
                None => write!(w, "[??]")?,
            }
            write!(w, "\t")?;
            Type::print_ref_from_offset(w, state, self.return_type)?;
        }
        Ok(())
    }

    fn print_parameters_label(&self, w: &mut Write) -> Result<()> {
        if !self.parameters.is_empty() {
            write!(w, "parameters:")?;
        }
        Ok(())
    }

    fn print_parameters(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        for parameter in &self.parameters {
            state.line(w, |w, state| Self::print_parameter(w, state, parameter))?;
        }
        Ok(())
    }

    fn diff_parameters(
        w: &mut Write,
        state: &mut DiffState,
        a: &Subprogram,
        b: &Subprogram,
    ) -> Result<()> {
        state.list(w,
                   &a.parameters,
                   (),
                   &mut (),
                   &b.parameters,
                   (),
                   &mut (),
                   Self::parameter_step_cost(),
                   Self::parameter_diff_cost,
        |w, state, x, _, _| state.line(w, |w, state| Self::print_parameter(w, state, x)),
        |w, state, a, _, _, b, _, _| state.line(w, a, b, |w, state, x| Self::print_parameter(w, state, x)))?;
        Ok(())
    }

    fn print_parameter(w: &mut Write, state: &PrintState, parameter: &Parameter) -> Result<()> {
        match parameter.byte_size(state.hash) {
            Some(byte_size) => write!(w, "[{}]", byte_size)?,
            None => write!(w, "[??]")?,
        }
        write!(w, "\t")?;
        parameter.print(w, state)
    }

    fn parameter_step_cost() -> usize {
        1
    }

    fn parameter_diff_cost(state: &DiffState, a: &Parameter, b: &Parameter) -> usize {
        let mut cost = 0;
        if a.name.cmp(&b.name) != cmp::Ordering::Equal {
            cost += 1;
        }
        match (a.ty(state.a.hash), b.ty(state.b.hash)) {
            (Some(ty_a), Some(ty_b)) => {
                if Type::cmp_id(state.a.hash, ty_a, state.b.hash, ty_b) != cmp::Ordering::Equal {
                    cost += 1;
                }
            }
            (None, None) => {}
            _ => {
                cost += 1;
            }
        }
        cost
    }

    fn print_variables_label(&self, w: &mut Write) -> Result<()> {
        if !self.variables.is_empty() {
            write!(w, "variables:")?;
        }
        Ok(())
    }

    fn print_variables(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        for variable in &self.variables {
            state.line(w, |w, state| Self::print_variable(w, state, variable))?;
        }
        Ok(())
    }

    fn diff_variables(
        w: &mut Write,
        state: &mut DiffState,
        a: &Subprogram,
        b: &Subprogram,
    ) -> Result<()> {
        let mut variables_a: Vec<_> = a.variables.iter().collect();
        variables_a.sort_by(|x, y| Variable::cmp_id(x, y));
        let mut variables_b: Vec<_> = b.variables.iter().collect();
        variables_b.sort_by(|x, y| Variable::cmp_id(x, y));
        state.list(w,
                   &variables_a,
                   (),
                   &mut (),
                   &variables_b,
                   (),
                   &mut (),
                   Self::variable_step_cost(),
                   |state, a, b| Self::variable_diff_cost(state, a, b),
        |w, state, x, _, _| state.line(w, |w, state| Self::print_variable(w, state, x)),
        |w, state, a, _, _, b, _, _| state.line(w, a, b, |w, state, x| Self::print_variable(w, state, x)))?;
        Ok(())
    }

    fn print_variable(w: &mut Write, state: &mut PrintState, variable: &Variable) -> Result<()> {
        match variable.byte_size(state.hash) {
            Some(byte_size) => write!(w, "[{}]", byte_size)?,
            None => write!(w, "[??]")?,
        }
        write!(w, "\t")?;
        variable.print_decl(w, state)
    }

    fn variable_step_cost() -> usize {
        1
    }

    fn variable_diff_cost(state: &DiffState, a: &Variable, b: &Variable) -> usize {
        let mut cost = 0;
        if a.name.cmp(&b.name) != cmp::Ordering::Equal {
            cost += 1;
        }
        match (a.ty(state.a.hash), b.ty(state.b.hash)) {
            (Some(ty_a), Some(ty_b)) => {
                if Type::cmp_id(state.a.hash, ty_a, state.b.hash, ty_b) != cmp::Ordering::Equal {
                    cost += 1;
                }
            }
            (None, None) => {}
            _ => {
                cost += 1;
            }
        }
        cost
    }

    fn print_inlined_subroutines_label(&self, w: &mut Write) -> Result<()> {
        if !self.inlined_subroutines.is_empty() {
            write!(w, "inlined subroutines:")?;
        }
        Ok(())
    }

    fn print_inlined_subroutines(
        &self,
        w: &mut Write,
        state: &mut PrintState,
        unit: &Unit,
    ) -> Result<()> {
        for subroutine in &self.inlined_subroutines {
            subroutine.print(w, state, unit, 1)?;
        }
        Ok(())
    }

    fn print_calls_label(&self, w: &mut Write) -> Result<()> {
        write!(w, "calls:")?;
        Ok(())
    }

    fn print_calls(&self, w: &mut Write, state: &mut PrintState, calls: &[u64]) -> Result<()> {
        for call in calls {
            state.line(w, |w, state| {
                write!(w, "0x{:x}", call)?;
                if let Some(subprogram) = state.hash.subprograms.get(call) {
                    write!(w, " ")?;
                    subprogram.print_ref(w)?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    fn diff_calls(
        _w: &mut Write,
        _state: &mut DiffState,
        _calls_a: &[u64],
        _calls_b: &[u64],
    ) -> Result<()> {
        // TODO
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Parameter<'input> {
    name: Option<&'input [u8]>,
    ty: Option<TypeOffset>,
}

impl<'input> Parameter<'input> {
    fn ty<'a>(&self, hash: &'a FileHash<'a, 'input>) -> Option<&'a Type<'input>>
        where 'input: 'a
    {
        self.ty.and_then(|v| Type::from_offset(hash, v))
    }

    fn byte_size(&self, hash: &FileHash) -> Option<u64> {
        self.ty(hash).and_then(|v| v.byte_size(hash))
    }

    fn print(&self, w: &mut Write, state: &PrintState) -> Result<()> {
        if let Some(name) = self.name {
            write!(w, "{}: ", String::from_utf8_lossy(name))?;
        }
        match self.ty(state.hash) {
            Some(ty) => ty.print_ref(w, state)?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    /// Compare the identifying information of two types.
    /// This can be used to sort, and to determine if two types refer to the same definition
    /// (even if there are differences in the definitions).
    fn cmp_id(hash_a: &FileHash, a: &Parameter, hash_b: &FileHash, b: &Parameter) -> cmp::Ordering {
        match (a.ty(hash_a), b.ty(hash_b)) {
            (Some(ty_a), Some(ty_b)) => {
                let ord = Type::cmp_id(hash_a, ty_a, hash_b, ty_b);
                if ord != cmp::Ordering::Equal {
                    return ord;
                }
            }
            (Some(_), None) => {
                return cmp::Ordering::Less;
            }
            (None, Some(_)) => {
                return cmp::Ordering::Greater;
            }
            (None, None) => {}
        }
        a.name.cmp(&b.name)
    }
}

#[derive(Debug, Default)]
struct InlinedSubroutine<'input> {
    abstract_origin: Option<SubprogramOffset>,
    size: Option<u64>,
    inlined_subroutines: Vec<InlinedSubroutine<'input>>,
    variables: Vec<Variable<'input>>,
}

impl<'input> InlinedSubroutine<'input> {
    fn print(
        &self,
        w: &mut Write,
        state: &mut PrintState,
        unit: &Unit,
        depth: usize,
    ) -> Result<()> {
        state.line(w, |w, state| self.print_name(w, state, unit))?;

        if state.flags.inline_depth > depth {
            state.indent(|state| {
                for subroutine in &self.inlined_subroutines {
                    subroutine.print(w, state, unit, depth + 1)?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    fn print_name(&self, w: &mut Write, _state: &PrintState, unit: &Unit) -> Result<()> {
        match self.size {
            Some(size) => write!(w, "[{}]", size)?,
            None => write!(w, "[??]")?,
        }
        write!(w, "\t")?;
        match self.abstract_origin.and_then(|v| Subprogram::from_offset(unit, v)) {
            Some(subprogram) => subprogram.print_ref(w)?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn diff(
        w: &mut Write,
        state: &mut DiffState,
        unit_a: &Unit,
        a: &InlinedSubroutine,
        unit_b: &Unit,
        b: &InlinedSubroutine,
        depth: usize,
    ) -> Result<()> {
        state.line(w, (unit_a, a), (unit_b, b), |w, state, (unit, x)| x.print_name(w, state, unit))?;

        if state.flags.inline_depth > depth {
            Self::diff_inlined_subroutines(w, state, unit_a, &a.inlined_subroutines, unit_b, &b.inlined_subroutines, 1)?;
        }

        Ok(())
    }

    fn diff_inlined_subroutines(
        w: &mut Write,
        state: &mut DiffState,
        unit_a: &Unit,
        a: &[InlinedSubroutine],
        unit_b: &Unit,
        b: &[InlinedSubroutine],
        depth: usize,
    ) -> Result<()> {
        state.indent(|state| {
            state.list(w,
                       a,
                       unit_a,
                       &mut (),
                       b,
                       unit_b,
                       &mut (),
                       InlinedSubroutine::step_cost(),
                       |state, a, b| InlinedSubroutine::diff_cost(state, unit_a, a, unit_b, b),
            |w, state, x, unit, _| x.print(w, state, unit, depth + 1),
            |w, state, a, unit_a, _, b, unit_b, _| InlinedSubroutine::diff(w, state, unit_a, a, unit_b, b, depth + 1))
        })
    }

    fn step_cost() -> usize {
        1
    }

    fn diff_cost(_state: &DiffState, unit_a: &Unit, a: &InlinedSubroutine, unit_b: &Unit, b: &InlinedSubroutine) -> usize {
        let mut cost = 0;
        let subprogram_a = a.abstract_origin.and_then(|v| Subprogram::from_offset(unit_a, v)).unwrap();
        let subprogram_b = b.abstract_origin.and_then(|v| Subprogram::from_offset(unit_b, v)).unwrap();
        if Subprogram::cmp_id(subprogram_a, subprogram_b) != cmp::Ordering::Equal {
            cost += 1;
        }
        if a.size != b.size {
            cost += 1;
        }
        cost
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct VariableOffset(usize);

impl From<gimli::DebugInfoOffset> for VariableOffset {
    fn from(o: gimli::DebugInfoOffset) -> VariableOffset {
        VariableOffset(o.0)
    }
}

#[derive(Debug, Default)]
struct Variable<'input> {
    namespace: Option<Rc<Namespace<'input>>>,
    name: Option<&'input [u8]>,
    linkage_name: Option<&'input [u8]>,
    ty: Option<TypeOffset>,
    declaration: bool,
    address: Option<u64>,
}

impl<'input> Variable<'input> {
    fn ty<'a>(&self, hash: &'a FileHash<'a, 'input>) -> Option<&'a Type<'input>>
        where 'input: 'a
    {
        self.ty.and_then(|v| Type::from_offset(hash, v))
    }

    fn byte_size(&self, hash: &FileHash) -> Option<u64> {
        self.ty(hash).and_then(|t| t.byte_size(hash))
    }

    fn filter(&self, flags: &Flags) -> bool {
        flags.filter_name(self.name) && flags.filter_namespace(&self.namespace)
    }

    /// Compare the identifying information of two variables.
    /// This can be used to sort, and to determine if two variables refer to the same definition
    /// (even if there are differences in the definitions).
    fn cmp_id(a: &Variable, b: &Variable) -> cmp::Ordering {
        cmp_ns_and_name(&a.namespace, a.name, &b.namespace, b.name)
    }

    /// Compare the size of two variables.
    fn cmp_size(hash_a: &FileHash, a: &Variable, hash_b: &FileHash, b: &Variable) -> cmp::Ordering {
        a.byte_size(hash_a).cmp(&b.byte_size(hash_b))
    }

    fn print_ref(&self, w: &mut Write) -> Result<()> {
        if let Some(ref namespace) = self.namespace {
            namespace.print(w)?;
        }
        match self.name {
            Some(name) => write!(w, "{}", String::from_utf8_lossy(name))?,
            None => write!(w, "<anon>")?,
        }
        Ok(())
    }

    fn print_decl(&self, w: &mut Write, state: &PrintState) -> Result<()> {
        self.print_ref(w)?;
        write!(w, ": ")?;
        Type::print_ref_from_offset(w, state, self.ty)?;
        Ok(())
    }

    fn print(&self, w: &mut Write, state: &mut PrintState) -> Result<()> {
        state.line(w, |w, state| self.print_name(w, state))?;
        state.indent(|state| {
            state.line_option(w, |w, _state| self.print_linkage_name(w))?;
            state.line_option(w, |w, _state| self.print_address(w))?;
            state.line_option(w, |w, state| self.print_size(w, state))?;
            state.line_option(w, |w, _state| self.print_declaration(w))
            // TODO: print anon type inline
        })
    }

    fn diff(w: &mut Write, state: &mut DiffState, a: &Variable, b: &Variable) -> Result<()> {
        state.line(w, a, b, |w, state, x| x.print_name(w, state))?;
        state.indent(|state| {
            state.line_option(w, a, b, |w, _state, x| x.print_linkage_name(w))?;
            let flag = state.flags.ignore_variable_address;
            state.ignore_diff(
                flag,
                |state| state.line_option(w, a, b, |w, _state, x| x.print_address(w)),
            )?;
            state.line_option(w, a, b, |w, state, x| x.print_size(w, state))?;
            state.line_option(w, a, b, |w, _state, x| x.print_declaration(w))
        })
    }

    fn print_name(&self, w: &mut Write, state: &PrintState) -> Result<()> {
        write!(w, "var ")?;
        self.print_ref(w)?;
        write!(w, ": ")?;
        Type::print_ref_from_offset(w, state, self.ty)?;
        Ok(())
    }

    fn print_linkage_name(&self, w: &mut Write) -> Result<()> {
        if let Some(linkage_name) = self.linkage_name {
            write!(w, "linkage name: {}", String::from_utf8_lossy(linkage_name))?;
        }
        Ok(())
    }

    fn print_address(&self, w: &mut Write) -> Result<()> {
        if let Some(address) = self.address {
            write!(w, "address: 0x{:x}", address)?;
        }
        Ok(())
    }

    fn print_size(&self, w: &mut Write, state: &PrintState) -> Result<()> {
        if let Some(byte_size) = self.byte_size(state.hash) {
            write!(w, "size: {}", byte_size)?;
        } else if !self.declaration {
            debug!("variable with no size");
        }
        Ok(())
    }

    fn print_declaration(&self, w: &mut Write) -> Result<()> {
        if self.declaration {
            write!(w, "declaration: yes")?;
        }
        Ok(())
    }
}

fn disassemble(code: &CodeRegion, low_pc: u64, high_pc: u64) -> Vec<u64> {
    match code.machine {
        goblin::elf::header::EM_X86_64 => {
            disassemble_arch::<amd64::Amd64>(&code.region, low_pc, high_pc, amd64::Mode::Long)
        }
        _ => Vec::new(),
    }
}

fn disassemble_arch<A>(
    region: &panopticon::Region,
    low_pc: u64,
    high_pc: u64,
    cfg: A::Configuration,
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
            /*
            //writeln!(w, "\t{:?}", mnemonic);
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
