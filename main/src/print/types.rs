use std::cmp;

use file::FileHash;
use print::{self, DiffState, Print, PrintState, SortList, ValuePrinter};
use types::{
    ArrayType, BaseType, FunctionType, PointerToMemberType, Type, TypeKind, TypeModifier,
    TypeModifierKind, TypeOffset, UnspecifiedType,
};
use unit::Unit;
use {Options, Result, Sort};

pub(crate) fn print(ty: &Type, state: &mut PrintState, unit: &Unit) -> Result<()> {
    let id = ty.id.get();
    match ty.kind {
        TypeKind::Def(ref val) => print::type_def::print(val, state, unit, id),
        TypeKind::Struct(ref val) => print::struct_type::print(val, state, unit, id),
        TypeKind::Union(ref val) => print::union_type::print(val, state, unit, id),
        TypeKind::Enumeration(ref val) => {
            print::enumeration::print(val, state, unit, id, ty.offset)
        }
        TypeKind::Base(..)
        | TypeKind::Array(..)
        | TypeKind::Function(..)
        | TypeKind::Unspecified(..)
        | TypeKind::PointerToMember(..)
        | TypeKind::Modifier(..) => Err(format!("can't print {:?}", ty).into()),
    }
}

pub(crate) fn print_ref(ty: &Type, w: &mut ValuePrinter, hash: &FileHash) -> Result<()> {
    let id = ty.id.get();
    match ty.kind {
        TypeKind::Base(ref val) => print_ref_base(val, w),
        TypeKind::Def(ref val) => print::type_def::print_ref(val, w, id),
        TypeKind::Struct(ref val) => print::struct_type::print_ref(val, w, id),
        TypeKind::Union(ref val) => print::union_type::print_ref(val, w, id),
        TypeKind::Enumeration(ref val) => print::enumeration::print_ref(val, w, id),
        TypeKind::Array(ref val) => print_ref_array(val, w, hash),
        TypeKind::Function(ref val) => print_ref_function(val, w, hash),
        TypeKind::Unspecified(ref val) => print_ref_unspecified(val, w),
        TypeKind::PointerToMember(ref val) => print_ref_pointer_to_member(val, w, hash),
        TypeKind::Modifier(ref val) => print_ref_modifier(val, w, hash),
    }
}

fn print_ref_base(ty: &BaseType, w: &mut ValuePrinter) -> Result<()> {
    write!(w, "{}", ty.name().unwrap_or("<anon-base-type>"))?;
    Ok(())
}

fn print_ref_array(ty: &ArrayType, w: &mut ValuePrinter, hash: &FileHash) -> Result<()> {
    write!(w, "[")?;
    print_ref_from_offset(w, hash, ty.ty)?;
    if let Some(count) = ty.count(hash) {
        write!(w, "; {}", count)?;
    }
    write!(w, "]")?;
    Ok(())
}

fn print_ref_function(ty: &FunctionType, w: &mut ValuePrinter, hash: &FileHash) -> Result<()> {
    let mut first = true;
    write!(w, "(")?;
    for parameter in &ty.parameters {
        if first {
            first = false;
        } else {
            write!(w, ", ")?;
        }
        print::parameter::print_decl(parameter, w, hash)?;
    }
    write!(w, ")")?;

    if let Some(return_type) = ty.return_type(hash) {
        write!(w, " -> ")?;
        print_ref(&return_type, w, hash)?;
    }
    Ok(())
}

fn print_ref_unspecified(ty: &UnspecifiedType, w: &mut ValuePrinter) -> Result<()> {
    if let Some(ref namespace) = ty.namespace {
        namespace.print(w)?;
    }
    write!(w, "{}", ty.name().unwrap_or("<void>"))?;
    Ok(())
}

fn print_ref_pointer_to_member(
    ty: &PointerToMemberType,
    w: &mut ValuePrinter,
    hash: &FileHash,
) -> Result<()> {
    print_ref_from_offset(w, hash, ty.containing_ty)?;
    write!(w, "::* ")?;
    print_ref_from_offset(w, hash, ty.ty)?;
    Ok(())
}

fn print_ref_modifier(ty: &TypeModifier, w: &mut ValuePrinter, hash: &FileHash) -> Result<()> {
    if let Some(name) = ty.name() {
        write!(w, "{}", name)?;
    } else {
        match ty.kind {
            TypeModifierKind::Pointer => write!(w, "* ")?,
            TypeModifierKind::Reference | TypeModifierKind::RvalueReference => write!(w, "& ")?,
            TypeModifierKind::Const => write!(w, "const ")?,
            TypeModifierKind::Volatile => write!(w, "volatile ")?,
            TypeModifierKind::Restrict => write!(w, "restrict ")?,
            TypeModifierKind::Packed
            | TypeModifierKind::Shared
            | TypeModifierKind::Atomic
            | TypeModifierKind::Other => {}
        }
        print_ref_from_offset(w, hash, ty.ty)?;
    }
    Ok(())
}

pub(crate) fn print_ref_from_offset(
    w: &mut ValuePrinter,
    hash: &FileHash,
    offset: TypeOffset,
) -> Result<()> {
    if offset.is_none() {
        write!(w, "void")?;
    } else {
        match Type::from_offset(hash, offset) {
            Some(ty) => print_ref(&ty, w, hash)?,
            None => write!(w, "<invalid-type {:?}>", offset)?,
        }
    }
    Ok(())
}

pub(crate) fn diff(
    state: &mut DiffState,
    unit_a: &Unit,
    type_a: &Type,
    unit_b: &Unit,
    type_b: &Type,
) -> Result<()> {
    use self::TypeKind::*;
    let id = type_a.id.get();
    match (&type_a.kind, &type_b.kind) {
        (&Def(ref a), &Def(ref b)) => print::type_def::diff(state, id, unit_a, a, unit_b, b),
        (&Struct(ref a), &Struct(ref b)) => {
            print::struct_type::diff(state, id, unit_a, a, unit_b, b)
        }
        (&Union(ref a), &Union(ref b)) => print::union_type::diff(state, id, unit_a, a, unit_b, b),
        (&Enumeration(ref a), &Enumeration(ref b)) => print::enumeration::diff(
            state,
            id,
            unit_a,
            a,
            type_a.offset,
            unit_b,
            b,
            type_b.offset,
        ),
        _ => Err(format!("can't diff {:?}, {:?}", type_a, type_b).into()),
    }?;
    Ok(())
}

pub(crate) fn print_members(state: &mut PrintState, unit: &Unit, ty: Option<&Type>) -> Result<()> {
    if let Some(ty) = ty {
        match ty.kind {
            TypeKind::Struct(ref t) => return print::struct_type::print_members(t, state, unit),
            TypeKind::Union(ref t) => return print::union_type::print_members(t, state, unit),
            _ => return Err(format!("can't print members {:?}", ty).into()),
        }
    }
    Ok(())
}

pub(crate) fn diff_members(
    state: &mut DiffState,
    unit_a: &Unit,
    type_a: Option<&Type>,
    unit_b: &Unit,
    type_b: Option<&Type>,
) -> Result<()> {
    if let (Some(type_a), Some(type_b)) = (type_a, type_b) {
        match (&type_a.kind, &type_b.kind) {
            (&TypeKind::Struct(ref a), &TypeKind::Struct(ref b)) => {
                return print::struct_type::diff_members(state, unit_a, a, unit_b, b);
            }
            (&TypeKind::Union(ref a), &TypeKind::Union(ref b)) => {
                return print::union_type::diff_members(state, unit_a, a, unit_b, b);
            }
            _ => {}
        }
    }

    // Different types, so don't try to diff the members.
    state.block((unit_a, type_a), (unit_b, type_b), |state, (unit, x)| {
        print_members(state, unit, x)
    })
}

impl<'input> Print for Type<'input> {
    type Arg = Unit<'input>;

    fn print(&self, state: &mut PrintState, unit: &Self::Arg) -> Result<()> {
        print(self, state, unit)
    }

    fn diff(
        state: &mut DiffState,
        unit_a: &Self::Arg,
        a: &Self,
        unit_b: &Self::Arg,
        b: &Self,
    ) -> Result<()> {
        diff(state, unit_a, a, unit_b, b)
    }
}

impl<'input> SortList for Type<'input> {
    /// This must only be called for types that have identifiers.
    fn cmp_id(
        hash_a: &FileHash,
        type_a: &Type,
        hash_b: &FileHash,
        type_b: &Type,
        _options: &Options,
    ) -> cmp::Ordering {
        Type::cmp_id(hash_a, type_a, hash_b, type_b)
    }

    fn cmp_by(
        hash_a: &FileHash,
        a: &Self,
        hash_b: &FileHash,
        b: &Self,
        options: &Options,
    ) -> cmp::Ordering {
        match options.sort {
            Sort::None => a.offset.cmp(&b.offset),
            Sort::Name => Type::cmp_id(hash_a, a, hash_b, b),
            Sort::Size => a.byte_size(hash_a).cmp(&b.byte_size(hash_b)),
        }
    }
}
