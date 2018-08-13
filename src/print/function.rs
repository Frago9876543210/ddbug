use std::cmp;
use std::fmt::Debug;

use amd64;
use panopticon;

use file::{CodeRegion, File, FileHash};
use function::{Function, Parameter};
use namespace::Namespace;
use print::{self, DiffList, DiffState, Print, PrintState, SortList, ValuePrinter};
use range::Range;
use unit::Unit;
use variable::LocalVariable;
use {Options, Result, Sort};

pub(crate) fn print_ref(f: &Function, w: &mut ValuePrinter) -> Result<()> {
    w.link(f.id.get(), &mut |w| {
        if let Some(ref namespace) = f.namespace {
            namespace.print(w)?;
        }
        write!(w, "{}", f.name().unwrap_or("<anon>"))?;
        Ok(())
    })
}

fn print_name(f: &Function, w: &mut ValuePrinter) -> Result<()> {
    write!(w, "fn ")?;
    if let Some(ref namespace) = f.namespace {
        namespace.print(w)?;
    }
    write!(w, "{}", f.name().unwrap_or("<anon>"))?;
    Ok(())
}

fn print_linkage_name(f: &Function, w: &mut ValuePrinter) -> Result<()> {
    if let Some(linkage_name) = f.linkage_name() {
        write!(w, "{}", linkage_name)?;
    }
    Ok(())
}

fn print_symbol_name(f: &Function, w: &mut ValuePrinter) -> Result<()> {
    if let Some(symbol_name) = f.symbol_name() {
        write!(w, "{}", symbol_name)?;
    }
    Ok(())
}

fn print_source(f: &Function, w: &mut ValuePrinter, unit: &Unit) -> Result<()> {
    if f.source.is_some() {
        f.source.print(w, unit)?;
    }
    Ok(())
}

fn print_address(f: &Function, w: &mut ValuePrinter) -> Result<()> {
    if let Some(range) = f.range() {
        range.print_address(w)?;
    }
    Ok(())
}

fn print_size(f: &Function, w: &mut ValuePrinter) -> Result<()> {
    if let Some(size) = f.size() {
        write!(w, "{}", size)?;
    }
    Ok(())
}

fn print_inline(f: &Function, w: &mut ValuePrinter) -> Result<()> {
    if f.inline {
        write!(w, "yes")?;
    }
    Ok(())
}

fn print_declaration(f: &Function, w: &mut ValuePrinter) -> Result<()> {
    if f.declaration {
        write!(w, "yes")?;
    }
    Ok(())
}

fn print_return_type(f: &Function, w: &mut ValuePrinter, hash: &FileHash) -> Result<()> {
    if f.return_type.is_some() {
        match f.return_type(hash).and_then(|t| t.byte_size(hash)) {
            Some(byte_size) => write!(w, "[{}]", byte_size)?,
            None => write!(w, "[??]")?,
        }
        write!(w, "\t")?;
        print::types::print_ref_from_offset(w, hash, f.return_type)?;
    }
    Ok(())
}

impl<'input> Print for Function<'input> {
    type Arg = Unit<'input>;

    fn print(&self, state: &mut PrintState, unit: &Self::Arg) -> Result<()> {
        state.collapsed(
            |state| state.id(self.id.get(), |w, _state| print_name(self, w)),
            |state| {
                state.field("linkage name", |w, _state| print_linkage_name(self, w))?;
                state.field("symbol name", |w, _state| print_symbol_name(self, w))?;
                if state.options().print_source {
                    state.field("source", |w, _state| print_source(self, w, unit))?;
                }
                state.field("address", |w, _state| print_address(self, w))?;
                state.field("size", |w, _state| print_size(self, w))?;
                state.field("inline", |w, _state| print_inline(self, w))?;
                state.field("declaration", |w, _state| print_declaration(self, w))?;
                state.field_expanded("return type", |state| {
                    state.line(|w, state| print_return_type(self, w, state))
                })?;
                state.field_expanded("parameters", |state| state.list(unit, &self.parameters))?;
                let details = state.hash().file.get_function_details(self.offset);
                if state.options().print_function_variables {
                    state.field_collapsed("variables", |state| {
                        state.list(unit, &details.variables)
                    })?;
                }
                state.inline(|state| {
                    state.field_collapsed("inlined functions", |state| {
                        state.list(unit, &details.inlined_functions)
                    })
                })?;
                if state.options().print_function_calls {
                    let calls = calls(self, state.hash().file);
                    state.field_collapsed("calls", |state| state.list(&(), &calls))?;
                }
                Ok(())
            },
        )?;
        state.line_break()?;
        Ok(())
    }

    fn diff(
        state: &mut DiffState,
        unit_a: &Self::Arg,
        a: &Self,
        unit_b: &Self::Arg,
        b: &Self,
    ) -> Result<()> {
        state.collapsed(
            |state| state.id(a.id.get(), a, b, |w, _state, x| print_name(x, w)),
            |state| {
                state.field("linkage name", a, b, |w, _state, x| {
                    print_linkage_name(x, w)
                })?;
                let flag = state.options().ignore_function_symbol_name;
                state.ignore_diff(flag, |state| {
                    state.field("symbol name", a, b, |w, _state, x| print_symbol_name(x, w))
                })?;
                if state.options().print_source {
                    state.field(
                        "source",
                        (unit_a, a),
                        (unit_b, b),
                        |w, _state, (unit, x)| print_source(x, w, unit),
                    )?;
                }
                let flag = state.options().ignore_function_address;
                state.ignore_diff(flag, |state| {
                    state.field("address", a, b, |w, _state, x| print_address(x, w))
                })?;
                let flag = state.options().ignore_function_size;
                state.ignore_diff(flag, |state| {
                    state.field("size", a, b, |w, _state, x| print_size(x, w))
                })?;
                let flag = state.options().ignore_function_inline;
                state.ignore_diff(flag, |state| {
                    state.field("inline", a, b, |w, _state, x| print_inline(x, w))
                })?;
                state.field("declaration", a, b, |w, _state, x| print_declaration(x, w))?;
                state.field_expanded("return type", |state| {
                    state.line(a, b, |w, state, x| print_return_type(x, w, state))
                })?;
                state.field_expanded("parameters", |state| {
                    state.list(unit_a, &a.parameters, unit_b, &b.parameters)
                })?;
                let details_a = state.hash_a().file.get_function_details(a.offset);
                let details_b = state.hash_b().file.get_function_details(b.offset);
                if state.options().print_function_variables {
                    let mut variables_a: Vec<_> = details_a.variables.iter().collect();
                    variables_a.sort_by(|x, y| {
                        LocalVariable::cmp_id(state.hash_a(), x, state.hash_a(), y, state.options())
                    });
                    let mut variables_b: Vec<_> = details_b.variables.iter().collect();
                    variables_b.sort_by(|x, y| {
                        LocalVariable::cmp_id(state.hash_b(), x, state.hash_b(), y, state.options())
                    });
                    state.field_collapsed("variables", |state| {
                        state.list(unit_a, &variables_a, unit_b, &variables_b)
                    })?;
                }
                state.inline(|state| {
                    state.field_collapsed("inlined functions", |state| {
                        state.list(
                            unit_a,
                            &details_a.inlined_functions,
                            unit_b,
                            &details_b.inlined_functions,
                        )
                    })
                })?;
                if state.options().print_function_calls {
                    let calls_a = calls(a, state.hash_a().file);
                    let calls_b = calls(b, state.hash_b().file);
                    state.field_collapsed("calls", |state| {
                        state.list(&(), &calls_a, &(), &calls_b)
                    })?;
                }
                Ok(())
            },
        )?;
        state.line_break()?;
        Ok(())
    }
}

impl<'input> SortList for Function<'input> {
    fn cmp_id(
        _hash_a: &FileHash,
        a: &Self,
        _hash_b: &FileHash,
        b: &Self,
        _options: &Options,
    ) -> cmp::Ordering {
        Namespace::cmp_ns_and_name(&a.namespace, a.name(), &b.namespace, b.name())
    }

    // This function is a bit of a hack. We use it for sorting, but not for
    // equality, in the hopes that we'll get better results in the presence
    // of overloading, while still coping with changed function signatures.
    // TODO: do something smarter
    fn cmp_id_for_sort(
        hash_a: &FileHash,
        a: &Self,
        hash_b: &FileHash,
        b: &Self,
        options: &Options,
    ) -> cmp::Ordering {
        let ord = Self::cmp_id(hash_a, a, hash_b, b, options);
        if ord != cmp::Ordering::Equal {
            return ord;
        }

        for (parameter_a, parameter_b) in a.parameters.iter().zip(b.parameters.iter()) {
            let ord = Parameter::cmp_type(hash_a, parameter_a, hash_b, parameter_b);
            if ord != cmp::Ordering::Equal {
                return ord;
            }
        }

        a.parameters.len().cmp(&b.parameters.len())
    }

    fn cmp_by(
        hash_a: &FileHash,
        a: &Self,
        hash_b: &FileHash,
        b: &Self,
        options: &Options,
    ) -> cmp::Ordering {
        match options.sort {
            // TODO: sort by offset?
            Sort::None => a.address.cmp(&b.address),
            Sort::Name => Self::cmp_id_for_sort(hash_a, a, hash_b, b, options),
            Sort::Size => a.size.cmp(&b.size),
        }
    }
}

fn calls(f: &Function, file: &File) -> Vec<Call> {
    if let Some(range) = f.range() {
        if let Some(code) = file.code() {
            return disassemble(code, range);
        }
    }
    Vec::new()
}

fn disassemble(code: &CodeRegion, range: Range) -> Vec<Call> {
    match code.machine {
        panopticon::Machine::Amd64 => {
            disassemble_arch::<amd64::Amd64>(&code.region, range, amd64::Mode::Long)
        }
        _ => Vec::new(),
    }
}

fn disassemble_arch<A>(
    region: &panopticon::Region,
    range: Range,
    cfg: A::Configuration,
) -> Vec<Call>
where
    A: panopticon::Architecture + Debug,
    A::Configuration: Debug,
{
    let mut calls = Vec::new();
    let mut addr = range.begin;
    while addr < range.end {
        let m = match A::decode(region, addr, &cfg) {
            Ok(m) => m,
            Err(e) => {
                error!("failed to disassemble: {}", e);
                return calls;
            }
        };

        for mnemonic in m.mnemonics {
            for instruction in &mnemonic.instructions {
                match *instruction {
                    panopticon::Statement {
                        op: panopticon::Operation::Call(ref call),
                        ..
                    } => match *call {
                        panopticon::Rvalue::Constant { ref value, .. } => {
                            calls.push(Call {
                                from: mnemonic.area.start,
                                to: *value,
                            });
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            addr = mnemonic.area.end;
        }
    }
    calls
}

struct Call {
    from: u64,
    to: u64,
}

impl Call {
    fn print(&self, w: &mut ValuePrinter, hash: &FileHash, options: &Options) -> Result<()> {
        if !options.ignore_function_address {
            // FIXME: it would be nice to display this in a way that doesn't clutter the output
            // when diffing
            write!(w, "0x{:x} -> 0x{:x} ", self.from, self.to)?;
        }
        if let Some(function) = hash.functions_by_address.get(&self.to) {
            print_ref(function, w)?;
        } else if options.ignore_function_address {
            // We haven't displayed an address yet, so we need to display something.
            write!(w, "0x{:x}", self.to)?;
        }
        Ok(())
    }
}

impl Print for Call {
    type Arg = ();

    fn print(&self, state: &mut PrintState, _arg: &()) -> Result<()> {
        let options = state.options();
        state.line(|w, hash| self.print(w, hash, options))
    }

    fn diff(state: &mut DiffState, _arg_a: &(), a: &Self, _arg_b: &(), b: &Self) -> Result<()> {
        let options = state.options();
        state.line(a, b, |w, hash, x| x.print(w, hash, options))
    }
}

impl DiffList for Call {
    fn step_cost(&self, _state: &DiffState, _arg: &()) -> usize {
        1
    }

    fn diff_cost(state: &DiffState, _arg_a: &(), a: &Self, _arg_b: &(), b: &Self) -> usize {
        let mut cost = 0;
        match (
            state.hash_a().functions_by_address.get(&a.to),
            state.hash_b().functions_by_address.get(&b.to),
        ) {
            (Some(function_a), Some(function_b)) => {
                if Function::cmp_id(
                    state.hash_a(),
                    function_a,
                    state.hash_b(),
                    function_b,
                    state.options(),
                ) != cmp::Ordering::Equal
                {
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
}
