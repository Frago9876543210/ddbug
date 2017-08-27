use std::rc::Rc;
use std::mem;

use gimli;

use range::Range;
use Result;
use file::{Namespace, NamespaceKind, Unit};
use function::{Function, InlinedFunction, Parameter};
use types::{ArrayType, BaseType, EnumerationType, Enumerator, FunctionType, Member,
            PointerToMemberType, StructType, Type, TypeDef, TypeKind, TypeModifier,
            TypeModifierKind, TypeOffset, UnionType, UnspecifiedType};
use variable::Variable;

struct DwarfFileState<'input, Endian>
where
    Endian: gimli::Endianity,
{
    endian: Endian,
    debug_abbrev: gimli::DebugAbbrev<gimli::EndianBuf<'input, Endian>>,
    debug_info: gimli::DebugInfo<gimli::EndianBuf<'input, Endian>>,
    debug_line: gimli::DebugLine<gimli::EndianBuf<'input, Endian>>,
    debug_str: gimli::DebugStr<gimli::EndianBuf<'input, Endian>>,
    debug_ranges: gimli::DebugRanges<gimli::EndianBuf<'input, Endian>>,
}

struct DwarfUnitState<'state, 'input, Endian>
where
    Endian: 'state + gimli::Endianity,
    'input: 'state,
{
    header: &'state gimli::CompilationUnitHeader<gimli::EndianBuf<'input, Endian>>,
    abbrev: &'state gimli::Abbreviations,
    subprograms: Vec<DwarfSubprogram<'input>>,
    variables: Vec<DwarfVariable<'input>>,
}

struct DwarfSubprogram<'input> {
    offset: gimli::UnitOffset,
    specification: gimli::DebugInfoOffset,
    function: Function<'input>,
}

struct DwarfVariable<'input> {
    offset: gimli::UnitOffset,
    specification: Option<gimli::DebugInfoOffset>,
    variable: Variable<'input>,
}

pub(crate) fn parse<'input, Endian, F>(endian: Endian, get_section: F) -> Result<Vec<Unit<'input>>>
where
    Endian: gimli::Endianity,
    F: Fn(&str) -> &'input [u8],
{
    let debug_abbrev = get_section(".debug_abbrev");
    let debug_abbrev = gimli::DebugAbbrev::new(debug_abbrev, endian);
    let debug_info = get_section(".debug_info");
    let debug_info = gimli::DebugInfo::new(debug_info, endian);
    let debug_line = get_section(".debug_line");
    let debug_line = gimli::DebugLine::new(debug_line, endian);
    let debug_str = get_section(".debug_str");
    let debug_str = gimli::DebugStr::new(debug_str, endian);
    let debug_ranges = get_section(".debug_ranges");
    let debug_ranges = gimli::DebugRanges::new(debug_ranges, endian);

    let dwarf = DwarfFileState {
        endian: endian,
        debug_abbrev: debug_abbrev,
        debug_info: debug_info,
        debug_line: debug_line,
        debug_str: debug_str,
        debug_ranges: debug_ranges,
    };

    let mut units = Vec::new();
    let mut unit_headers = dwarf.debug_info.units();
    while let Some(unit_header) = unit_headers.next()? {
        units.push(parse_unit(&dwarf, &unit_header)?);
    }
    Ok(units)
}

fn parse_unit<'input, Endian>(
    dwarf: &DwarfFileState<'input, Endian>,
    unit_header: &gimli::CompilationUnitHeader<gimli::EndianBuf<'input, Endian>>,
) -> Result<Unit<'input>>
where
    Endian: gimli::Endianity,
{
    let abbrev = &unit_header.abbreviations(&dwarf.debug_abbrev)?;
    let mut dwarf_unit = DwarfUnitState {
        header: unit_header,
        abbrev: abbrev,
        subprograms: Vec::new(),
        variables: Vec::new(),
    };

    let mut tree = unit_header.entries_tree(abbrev, None)?;
    let root = tree.root()?;

    let mut unit = Unit::default();
    unit.address_size = Some(unit_header.address_size() as u64);
    {
        let entry = root.entry();
        if entry.tag() != gimli::DW_TAG_compile_unit {
            return Err(format!("unknown CU tag: {}", entry.tag()).into());
        }
        let mut attrs = entry.attrs();
        let mut stmt_list = None;
        let mut ranges = None;
        let mut high_pc = None;
        let mut size = None;
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_name => {
                    unit.name = attr.string_value(&dwarf.debug_str).map(|s| s.buf())
                }
                gimli::DW_AT_comp_dir => {
                    unit.dir = attr.string_value(&dwarf.debug_str).map(|s| s.buf())
                }
                gimli::DW_AT_language => {
                    if let gimli::AttributeValue::Language(language) = attr.value() {
                        unit.language = Some(language);
                    }
                }
                gimli::DW_AT_low_pc => if let gimli::AttributeValue::Addr(addr) = attr.value() {
                    // TODO: is address 0 ever valid?
                    if addr != 0 {
                        unit.low_pc = Some(addr);
                    }
                },
                gimli::DW_AT_high_pc => match attr.value() {
                    gimli::AttributeValue::Addr(val) => high_pc = Some(val),
                    gimli::AttributeValue::Udata(val) => size = Some(val),
                    val => debug!("unknown CU DW_AT_high_pc: {:?}", val),
                },
                gimli::DW_AT_stmt_list => {
                    if let gimli::AttributeValue::DebugLineRef(val) = attr.value() {
                        stmt_list = Some(val);
                    }
                }
                gimli::DW_AT_ranges => {
                    if let gimli::AttributeValue::DebugRangesRef(val) = attr.value() {
                        ranges = Some(val);
                    }
                }
                gimli::DW_AT_producer |
                gimli::DW_AT_entry_pc |
                gimli::DW_AT_APPLE_optimized |
                gimli::DW_AT_macro_info |
                gimli::DW_AT_sibling => {}
                _ => debug!("unknown CU attribute: {} {:?}", attr.name(), attr.value()),
            }
        }

        // Find ranges from attributes in order of preference:
        // DW_AT_stmt_list, DW_AT_ranges, DW_AT_high_pc, DW_AT_size.
        // TODO: include variables in ranges.
        if let Some(offset) = stmt_list {
            let comp_name = unit.name.map(|buf| gimli::EndianBuf::new(buf, dwarf.endian));
            let comp_dir = unit.dir.map(|buf| gimli::EndianBuf::new(buf, dwarf.endian));
            let mut rows = dwarf
                .debug_line
                .program(offset, dwarf_unit.header.address_size(), comp_name, comp_dir)?
                .rows();
            let mut seq_addr = None;
            while let Some((_, row)) = rows.next_row()? {
                let addr = row.address();
                if row.end_sequence() {
                    if let Some(seq_addr) = seq_addr {
                        // Sequences starting at 0 are probably invalid.
                        // TODO: is this always desired?
                        if seq_addr != 0 {
                            unit.ranges.push(Range {
                                begin: seq_addr,
                                end: addr,
                            });
                        }
                    }
                    seq_addr = None;
                } else if seq_addr.is_none() {
                    seq_addr = Some(addr);
                }
            }
        } else if let Some(offset) = ranges {
            let low_pc = unit.low_pc.unwrap_or(0);
            let mut ranges =
                dwarf.debug_ranges.ranges(offset, dwarf_unit.header.address_size(), low_pc)?;
            while let Some(range) = ranges.next()? {
                // Ranges starting at 0 are probably invalid.
                // TODO: is this always desired?
                if range.begin != 0 {
                    unit.ranges.push(Range {
                        begin: range.begin,
                        end: range.end,
                    });
                }
            }
        } else if let Some(low_pc) = unit.low_pc {
            if let Some(size) = size {
                if high_pc.is_none() {
                    high_pc = low_pc.checked_add(size);
                }
            }
            if let Some(high_pc) = high_pc {
                unit.ranges.push(Range {
                    begin: low_pc,
                    end: high_pc,
                });
            }
        }
        unit.ranges.sort();
    };

    let namespace = None;
    parse_namespace_children(&mut unit, dwarf, &mut dwarf_unit, &namespace, root.children())?;

    fixup_subprogram_specifications(&mut unit, dwarf, &mut dwarf_unit)?;
    fixup_variable_specifications(&mut unit, dwarf, &mut dwarf_unit)?;

    Ok(unit)
}

fn fixup_subprogram_specifications<'state, 'input, Endian>(
    unit: &mut Unit<'input>,
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
) -> Result<()>
where
    Endian: gimli::Endianity,
{
    let mut defer = Vec::new();

    while !dwarf_unit.subprograms.is_empty() {
        let mut progress = false;

        mem::swap(&mut defer, &mut dwarf_unit.subprograms);
        for mut subprogram in defer.drain(..) {
            if parse_subprogram_specification(
                unit,
                &mut subprogram.function,
                subprogram.specification,
            ) {
                let mut tree =
                    dwarf_unit.header.entries_tree(dwarf_unit.abbrev, Some(subprogram.offset))?;
                parse_subprogram_children(
                    unit,
                    dwarf,
                    dwarf_unit,
                    &mut subprogram.function,
                    tree.root()?.children(),
                )?;
                let offset = subprogram.offset.to_debug_info_offset(dwarf_unit.header);
                unit.functions.insert(offset.into(), subprogram.function);
                progress = true;
            } else {
                dwarf_unit.subprograms.push(subprogram);
            }
        }

        if !progress {
            debug!("invalid specification for {} subprograms", dwarf_unit.subprograms.len());
            mem::swap(&mut defer, &mut dwarf_unit.subprograms);
            for mut subprogram in defer.drain(..) {
                let mut tree =
                    dwarf_unit.header.entries_tree(dwarf_unit.abbrev, Some(subprogram.offset))?;
                parse_subprogram_children(
                    unit,
                    dwarf,
                    dwarf_unit,
                    &mut subprogram.function,
                    tree.root()?.children(),
                )?;
                let offset = subprogram.offset.to_debug_info_offset(dwarf_unit.header);
                unit.functions.insert(offset.into(), subprogram.function);
            }
            return Ok(());
        }
    }
    Ok(())
}

fn fixup_variable_specifications<'state, 'input, Endian>(
    unit: &mut Unit<'input>,
    _dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
) -> Result<()>
where
    Endian: gimli::Endianity,
{
    loop {
        let mut progress = false;
        let mut defer = Vec::new();

        for mut variable in dwarf_unit.variables.drain(..) {
            match variable.specification.and_then(|v| unit.variables.get(&v.into())) {
                Some(specification) => {
                    let variable = &mut variable.variable;
                    variable.namespace = specification.namespace.clone();
                    if variable.name.is_none() {
                        variable.name = specification.name;
                    }
                    if variable.linkage_name.is_none() {
                        variable.linkage_name = specification.linkage_name;
                    }
                    if variable.ty.is_none() {
                        variable.ty = specification.ty;
                    }
                }
                None => {
                    defer.push(variable);
                    continue;
                }
            }
            let offset = variable.offset.to_debug_info_offset(dwarf_unit.header);
            unit.variables.insert(offset.into(), variable.variable);
            progress = true;
        }

        if defer.is_empty() {
            return Ok(());
        }
        if !progress {
            debug!("invalid specification for {} variables", defer.len());
            for variable in dwarf_unit.variables.drain(..) {
                let offset = variable.offset.to_debug_info_offset(dwarf_unit.header);
                unit.variables.insert(offset.into(), variable.variable);
            }
            return Ok(());
        }
        dwarf_unit.variables = defer;
    }
}

fn parse_namespace_children<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    unit: &mut Unit<'input>,
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    namespace: &Option<Rc<Namespace<'input>>>,
    mut iter: gimli::EntriesTreeIter<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<()>
where
    Endian: gimli::Endianity,
{
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            gimli::DW_TAG_namespace => {
                parse_namespace(unit, dwarf, dwarf_unit, namespace, child)?;
            }
            gimli::DW_TAG_subprogram => {
                parse_subprogram(unit, dwarf, dwarf_unit, namespace, child)?;
            }
            gimli::DW_TAG_variable => {
                let variable = parse_variable(unit, dwarf, dwarf_unit, namespace.clone(), child)?;
                if variable.specification.is_some() {
                    // Delay handling specication in case it comes later.
                    dwarf_unit.variables.push(variable);
                } else {
                    let offset = variable.offset.to_debug_info_offset(dwarf_unit.header);
                    unit.variables.insert(offset.into(), variable.variable);
                }
            }
            gimli::DW_TAG_imported_declaration | gimli::DW_TAG_imported_module => {}
            tag => {
                let offset = child.entry().offset();
                let offset = offset.to_debug_info_offset(dwarf_unit.header);
                if let Some(ty) = parse_type(unit, dwarf, dwarf_unit, namespace, child)? {
                    unit.types.insert(offset.into(), ty);
                } else {
                    debug!("unknown namespace child tag: {}", tag);
                }
            }
        }
    }
    Ok(())
}

fn parse_namespace<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    unit: &mut Unit<'input>,
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    namespace: &Option<Rc<Namespace<'input>>>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<()>
where
    Endian: gimli::Endianity,
{
    let mut name = None;

    {
        let entry = node.entry();
        let mut attrs = entry.attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_name => {
                    name = attr.string_value(&dwarf.debug_str).map(|s| s.buf());
                }
                gimli::DW_AT_decl_file | gimli::DW_AT_decl_line => {}
                _ => debug!("unknown namespace attribute: {} {:?}", attr.name(), attr.value()),
            }
        }
    }

    let namespace = Some(Namespace::new(namespace, name, NamespaceKind::Namespace));
    parse_namespace_children(unit, dwarf, dwarf_unit, &namespace, node.children())
}

fn parse_type_offset<'state, 'input, Endian>(
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    attr: &gimli::Attribute<gimli::EndianBuf<'input, Endian>>,
) -> Option<TypeOffset>
where
    Endian: gimli::Endianity,
{
    match attr.value() {
        gimli::AttributeValue::UnitRef(offset) => {
            Some(offset.to_debug_info_offset(dwarf_unit.header).into())
        }
        gimli::AttributeValue::DebugInfoRef(offset) => Some(offset.into()),
        other => {
            debug!("unknown type offset: {:?}", other);
            None
        }
    }
}

fn parse_type<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    unit: &mut Unit<'input>,
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    namespace: &Option<Rc<Namespace<'input>>>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<Option<Type<'input>>>
where
    Endian: gimli::Endianity,
{
    let tag = node.entry().tag();
    let mut ty = Type::default();
    let offset = node.entry().offset();
    let offset = offset.to_debug_info_offset(dwarf_unit.header);
    ty.offset = offset.into();
    ty.kind = match tag {
        gimli::DW_TAG_base_type => {
            TypeKind::Base(parse_base_type(dwarf, dwarf_unit, namespace, node)?)
        }
        gimli::DW_TAG_typedef => TypeKind::Def(parse_typedef(dwarf, dwarf_unit, namespace, node)?),
        // TODO: distinguish between class and structure
        gimli::DW_TAG_class_type | gimli::DW_TAG_structure_type => {
            TypeKind::Struct(parse_structure_type(unit, dwarf, dwarf_unit, namespace, node)?)
        }
        gimli::DW_TAG_union_type => {
            TypeKind::Union(parse_union_type(unit, dwarf, dwarf_unit, namespace, node)?)
        }
        gimli::DW_TAG_enumeration_type => {
            TypeKind::Enumeration(parse_enumeration_type(unit, dwarf, dwarf_unit, namespace, node)?)
        }
        gimli::DW_TAG_array_type => {
            TypeKind::Array(parse_array_type(dwarf, dwarf_unit, namespace, node)?)
        }
        gimli::DW_TAG_subroutine_type => {
            TypeKind::Function(parse_subroutine_type(dwarf, dwarf_unit, namespace, node)?)
        }
        gimli::DW_TAG_unspecified_type => {
            TypeKind::Unspecified(parse_unspecified_type(dwarf, dwarf_unit, namespace, node)?)
        }
        gimli::DW_TAG_ptr_to_member_type => TypeKind::PointerToMember(
            parse_pointer_to_member_type(dwarf, dwarf_unit, namespace, node)?,
        ),
        gimli::DW_TAG_pointer_type => TypeKind::Modifier(
            parse_type_modifier(dwarf, dwarf_unit, node, TypeModifierKind::Pointer)?,
        ),
        gimli::DW_TAG_reference_type => TypeKind::Modifier(
            parse_type_modifier(dwarf, dwarf_unit, node, TypeModifierKind::Reference)?,
        ),
        gimli::DW_TAG_const_type => TypeKind::Modifier(
            parse_type_modifier(dwarf, dwarf_unit, node, TypeModifierKind::Const)?,
        ),
        gimli::DW_TAG_packed_type => TypeKind::Modifier(
            parse_type_modifier(dwarf, dwarf_unit, node, TypeModifierKind::Packed)?,
        ),
        gimli::DW_TAG_volatile_type => TypeKind::Modifier(
            parse_type_modifier(dwarf, dwarf_unit, node, TypeModifierKind::Volatile)?,
        ),
        gimli::DW_TAG_restrict_type => TypeKind::Modifier(
            parse_type_modifier(dwarf, dwarf_unit, node, TypeModifierKind::Restrict)?,
        ),
        gimli::DW_TAG_shared_type => TypeKind::Modifier(
            parse_type_modifier(dwarf, dwarf_unit, node, TypeModifierKind::Shared)?,
        ),
        gimli::DW_TAG_rvalue_reference_type => TypeKind::Modifier(
            parse_type_modifier(dwarf, dwarf_unit, node, TypeModifierKind::RvalueReference)?,
        ),
        gimli::DW_TAG_atomic_type => TypeKind::Modifier(
            parse_type_modifier(dwarf, dwarf_unit, node, TypeModifierKind::Atomic)?,
        ),
        _ => return Ok(None),
    };
    Ok(Some(ty))
}

fn parse_type_modifier<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
    kind: TypeModifierKind,
) -> Result<TypeModifier<'input>>
where
    Endian: gimli::Endianity,
{
    let mut modifier = TypeModifier {
        kind: kind,
        ty: None,
        name: None,
        byte_size: None,
        address_size: Some(dwarf_unit.header.address_size() as u64),
    };

    {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_name => {
                    modifier.name = attr.string_value(&dwarf.debug_str).map(|s| s.buf());
                }
                gimli::DW_AT_type => if let Some(offset) = parse_type_offset(dwarf_unit, &attr) {
                    modifier.ty = Some(offset);
                },
                gimli::DW_AT_byte_size => {
                    modifier.byte_size = attr.udata_value();
                }
                _ => debug!("unknown type modifier attribute: {} {:?}", attr.name(), attr.value()),
            }
        }
    }

    let mut iter = node.children();
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            tag => {
                debug!("unknown type modifier child tag: {}", tag);
            }
        }
    }
    Ok(modifier)
}

fn parse_base_type<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    dwarf: &DwarfFileState<'input, Endian>,
    _unit: &mut DwarfUnitState<'state, 'input, Endian>,
    _namespace: &Option<Rc<Namespace<'input>>>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<BaseType<'input>>
where
    Endian: gimli::Endianity,
{
    let mut ty = BaseType::default();

    {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_name => {
                    ty.name = attr.string_value(&dwarf.debug_str).map(|s| s.buf());
                }
                gimli::DW_AT_byte_size => {
                    ty.byte_size = attr.udata_value();
                }
                gimli::DW_AT_encoding => {}
                _ => debug!("unknown base type attribute: {} {:?}", attr.name(), attr.value()),
            }
        }
    }

    let mut iter = node.children();
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            tag => {
                debug!("unknown base type child tag: {}", tag);
            }
        }
    }
    Ok(ty)
}

fn parse_typedef<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    namespace: &Option<Rc<Namespace<'input>>>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<TypeDef<'input>>
where
    Endian: gimli::Endianity,
{
    let mut typedef = TypeDef::default();
    typedef.namespace = namespace.clone();

    {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_name => {
                    typedef.name = attr.string_value(&dwarf.debug_str).map(|s| s.buf());
                }
                gimli::DW_AT_type => if let Some(offset) = parse_type_offset(dwarf_unit, &attr) {
                    typedef.ty = Some(offset);
                },
                gimli::DW_AT_decl_file | gimli::DW_AT_decl_line => {}
                _ => debug!("unknown typedef attribute: {} {:?}", attr.name(), attr.value()),
            }
        }
    }

    let mut iter = node.children();
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            tag => {
                debug!("unknown typedef child tag: {}", tag);
            }
        }
    }
    Ok(typedef)
}

fn parse_structure_type<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    unit: &mut Unit<'input>,
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    namespace: &Option<Rc<Namespace<'input>>>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<StructType<'input>>
where
    Endian: gimli::Endianity,
{
    let mut ty = StructType::default();
    ty.namespace = namespace.clone();

    {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_name => {
                    ty.name = attr.string_value(&dwarf.debug_str).map(|s| s.buf());
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
                gimli::DW_AT_containing_type |
                gimli::DW_AT_alignment |
                gimli::DW_AT_sibling => {}
                _ => debug!("unknown struct attribute: {} {:?}", attr.name(), attr.value()),
            }
        }
    }

    let namespace = Some(Namespace::new(&ty.namespace, ty.name, NamespaceKind::Type));
    let mut iter = node.children();
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            gimli::DW_TAG_subprogram => {
                parse_subprogram(unit, dwarf, dwarf_unit, &namespace, child)?;
            }
            gimli::DW_TAG_member => {
                parse_member(&mut ty.members, unit, dwarf, dwarf_unit, &namespace, child)?;
            }
            gimli::DW_TAG_inheritance |
            gimli::DW_TAG_template_type_parameter |
            gimli::DW_TAG_template_value_parameter |
            gimli::DW_TAG_GNU_template_parameter_pack => {}
            tag => {
                let offset = child.entry().offset();
                let offset = offset.to_debug_info_offset(dwarf_unit.header);
                if let Some(ty) = parse_type(unit, dwarf, dwarf_unit, &namespace, child)? {
                    unit.types.insert(offset.into(), ty);
                } else {
                    debug!("unknown struct child tag: {}", tag);
                }
            }
        }
    }
    ty.members.sort_by_key(|v| v.bit_offset);
    let mut bit_offset = ty.byte_size.map(|v| v * 8);
    for member in ty.members.iter_mut().rev() {
        member.next_bit_offset = bit_offset;
        bit_offset = Some(member.bit_offset);
    }
    Ok(ty)
}

fn parse_union_type<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    unit: &mut Unit<'input>,
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    namespace: &Option<Rc<Namespace<'input>>>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<UnionType<'input>>
where
    Endian: gimli::Endianity,
{
    let mut ty = UnionType::default();
    ty.namespace = namespace.clone();

    {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_name => {
                    ty.name = attr.string_value(&dwarf.debug_str).map(|s| s.buf());
                }
                gimli::DW_AT_byte_size => {
                    ty.byte_size = attr.udata_value();
                }
                gimli::DW_AT_declaration => {
                    if let gimli::AttributeValue::Flag(flag) = attr.value() {
                        ty.declaration = flag;
                    }
                }
                gimli::DW_AT_alignment |
                gimli::DW_AT_decl_file |
                gimli::DW_AT_decl_line |
                gimli::DW_AT_sibling => {}
                _ => debug!("unknown union attribute: {} {:?}", attr.name(), attr.value()),
            }
        }
    }

    let namespace = Some(Namespace::new(&ty.namespace, ty.name, NamespaceKind::Type));
    let mut iter = node.children();
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            gimli::DW_TAG_subprogram => {
                parse_subprogram(unit, dwarf, dwarf_unit, &namespace, child)?;
            }
            gimli::DW_TAG_member => {
                parse_member(&mut ty.members, unit, dwarf, dwarf_unit, &namespace, child)?;
            }
            gimli::DW_TAG_template_type_parameter => {}
            tag => {
                let offset = child.entry().offset();
                let offset = offset.to_debug_info_offset(dwarf_unit.header);
                if let Some(ty) = parse_type(unit, dwarf, dwarf_unit, &namespace, child)? {
                    unit.types.insert(offset.into(), ty);
                } else {
                    debug!("unknown union child tag: {}", tag);
                }
            }
        }
    }
    Ok(ty)
}

fn parse_member<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    members: &mut Vec<Member<'input>>,
    unit: &mut Unit<'input>,
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    namespace: &Option<Rc<Namespace<'input>>>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<()>
where
    Endian: gimli::Endianity,
{
    let mut member = Member::default();
    let mut bit_offset = None;
    let mut byte_size = None;
    let mut declaration = false;

    {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_name => {
                    member.name = attr.string_value(&dwarf.debug_str).map(|s| s.buf());
                }
                gimli::DW_AT_type => if let Some(offset) = parse_type_offset(dwarf_unit, &attr) {
                    member.ty = Some(offset);
                },
                gimli::DW_AT_data_member_location => {
                    match attr.value() {
                        gimli::AttributeValue::Udata(v) => member.bit_offset = v * 8,
                        gimli::AttributeValue::Sdata(v) => if v >= 0 {
                            member.bit_offset = (v as u64) * 8;
                        } else {
                            debug!("DW_AT_data_member_location is negative: {}", v)
                        },
                        gimli::AttributeValue::Exprloc(expr) => {
                            match evaluate(dwarf_unit.header, expr) {
                                Some(gimli::Location::Address { address }) => {
                                    member.bit_offset = address * 8;
                                }
                                Some(gimli::Location::Register { .. }) | None => {}
                                Some(loc) => {
                                    debug!("unknown DW_AT_data_member_location result: {:?}", loc)
                                }
                            }
                        }
                        gimli::AttributeValue::DebugLocRef(..) => {
                            // TODO
                        }
                        _ => {
                            debug!("unknown DW_AT_data_member_location: {:?}", attr.value());
                        }
                    }
                }
                gimli::DW_AT_data_bit_offset => if let Some(bit_offset) = attr.udata_value() {
                    member.bit_offset = bit_offset;
                },
                gimli::DW_AT_bit_offset => {
                    bit_offset = attr.udata_value();
                }
                gimli::DW_AT_byte_size => {
                    byte_size = attr.udata_value();
                }
                gimli::DW_AT_bit_size => {
                    member.bit_size = attr.udata_value();
                }
                gimli::DW_AT_declaration => {
                    declaration = true;
                }
                gimli::DW_AT_decl_file |
                gimli::DW_AT_decl_line |
                gimli::DW_AT_external |
                gimli::DW_AT_accessibility |
                gimli::DW_AT_artificial |
                gimli::DW_AT_const_value |
                gimli::DW_AT_alignment |
                gimli::DW_AT_sibling => {}
                _ => {
                    debug!("unknown member attribute: {} {:?}", attr.name(), attr.value());
                }
            }
        }
    }

    if declaration {
        // This is a C++ static data member. Parse it as a variable instead.
        // Note: the DWARF 5 standard says static members should use DW_TAG_variable,
        // but at least clang 3.7.1 uses DW_TAG_member.
        let variable = parse_variable(unit, dwarf, dwarf_unit, namespace.clone(), node)?;
        if variable.specification.is_some() {
            debug!("specification on variable declaration at offset 0x{:x}", variable.offset.0);
        }
        let offset = variable.offset.to_debug_info_offset(dwarf_unit.header);
        unit.variables.insert(offset.into(), variable.variable);
        return Ok(());
    }

    if let (Some(bit_offset), Some(bit_size)) = (bit_offset, member.bit_size) {
        // DWARF version 2/3, but allowed in later versions for compatibility.
        // The data member is a bit field contained in an anonymous object.
        // member.bit_offset starts as the offset of the anonymous object.
        // byte_size is the size of the anonymous object.
        // bit_offset is the offset from the anonymous object MSB to the bit field MSB.
        // bit_size is the size of the bit field.
        if dwarf.endian.is_big_endian() {
            // For big endian, the MSB is the first bit, so we simply add bit_offset,
            // and byte_size is unneeded.
            member.bit_offset = member.bit_offset.wrapping_add(bit_offset);
        } else {
            // For little endian, we have to work backwards, so we need byte_size.
            if let Some(byte_size) = byte_size {
                // First find the offset of the MSB of the anonymous object.
                member.bit_offset = member.bit_offset.wrapping_add(byte_size * 8);
                // Then go backwards to the LSB of the bit field.
                member.bit_offset =
                    member.bit_offset.wrapping_sub(bit_offset.wrapping_add(bit_size));
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

    let mut iter = node.children();
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            tag => {
                debug!("unknown member child tag: {}", tag);
            }
        }
    }
    members.push(member);
    Ok(())
}

fn parse_enumeration_type<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    unit: &mut Unit<'input>,
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    namespace: &Option<Rc<Namespace<'input>>>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<EnumerationType<'input>>
where
    Endian: gimli::Endianity,
{
    let mut ty = EnumerationType::default();
    ty.namespace = namespace.clone();

    {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_name => {
                    ty.name = attr.string_value(&dwarf.debug_str).map(|s| s.buf());
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
                gimli::DW_AT_sibling |
                gimli::DW_AT_type |
                gimli::DW_AT_alignment |
                gimli::DW_AT_enum_class => {}
                _ => debug!("unknown enumeration attribute: {} {:?}", attr.name(), attr.value()),
            }
        }
    }

    let namespace = Some(Namespace::new(&ty.namespace, ty.name, NamespaceKind::Type));
    let mut iter = node.children();
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            gimli::DW_TAG_enumerator => {
                ty.enumerators.push(parse_enumerator(dwarf, dwarf_unit, &namespace, child)?);
            }
            gimli::DW_TAG_subprogram => {
                parse_subprogram(unit, dwarf, dwarf_unit, &namespace, child)?;
            }
            tag => {
                debug!("unknown enumeration child tag: {}", tag);
            }
        }
    }
    Ok(ty)
}

fn parse_enumerator<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    dwarf: &DwarfFileState<'input, Endian>,
    _dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    _namespace: &Option<Rc<Namespace<'input>>>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<Enumerator<'input>>
where
    Endian: gimli::Endianity,
{
    let mut enumerator = Enumerator::default();

    {
        let mut attrs = node.entry().attrs();
        while let Some(ref attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_name => {
                    enumerator.name = attr.string_value(&dwarf.debug_str).map(|s| s.buf());
                }
                gimli::DW_AT_const_value => if let Some(value) = attr.sdata_value() {
                    enumerator.value = Some(value);
                } else {
                    debug!("unknown enumerator const_value: {:?}", attr.value());
                },
                _ => debug!("unknown enumerator attribute: {} {:?}", attr.name(), attr.value()),
            }
        }
    }

    let mut iter = node.children();
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            tag => {
                debug!("unknown enumerator child tag: {}", tag);
            }
        }
    }
    Ok(enumerator)
}

fn parse_array_type<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    _dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    _namespace: &Option<Rc<Namespace<'input>>>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<ArrayType<'input>>
where
    Endian: gimli::Endianity,
{
    let mut array = ArrayType::default();

    {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_type => if let Some(offset) = parse_type_offset(dwarf_unit, &attr) {
                    array.ty = Some(offset);
                },
                gimli::DW_AT_byte_size => {
                    array.byte_size = attr.udata_value();
                }
                gimli::DW_AT_name | gimli::DW_AT_GNU_vector | gimli::DW_AT_sibling => {}
                _ => debug!("unknown array attribute: {} {:?}", attr.name(), attr.value()),
            }
        }
    }

    let mut iter = node.children();
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            gimli::DW_TAG_subrange_type => {
                let mut attrs = child.entry().attrs();
                while let Some(attr) = attrs.next()? {
                    match attr.name() {
                        gimli::DW_AT_count => {
                            array.count = attr.udata_value();
                        }
                        gimli::DW_AT_upper_bound => {
                            // byte_size takes precedence over upper_bound when
                            // determining the count.
                            if array.byte_size.is_none() {
                                if let Some(upper_bound) = attr.udata_value() {
                                    // TODO: use AT_lower_bound too (and default lower bound)
                                    if let Some(count) = u64::checked_add(upper_bound, 1) {
                                        array.count = Some(count);
                                    } else {
                                        debug!("overflow for array upper bound: {}", upper_bound);
                                    }
                                }
                            }
                        }
                        gimli::DW_AT_type | gimli::DW_AT_lower_bound => {}
                        _ => debug!(
                            "unknown array subrange attribute: {} {:?}",
                            attr.name(),
                            attr.value()
                        ),
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

fn parse_subroutine_type<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    _namespace: &Option<Rc<Namespace<'input>>>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<FunctionType<'input>>
where
    Endian: gimli::Endianity,
{
    let mut function = FunctionType {
        // Go treats subroutine types as pointers.
        // Not sure if this is valid for all languages.
        byte_size: Some(dwarf_unit.header.address_size() as u64),
        ..Default::default()
    };

    {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_type => if let Some(offset) = parse_type_offset(dwarf_unit, &attr) {
                    function.return_type = Some(offset);
                },
                gimli::DW_AT_name | gimli::DW_AT_prototyped | gimli::DW_AT_sibling => {}
                _ => debug!("unknown subroutine attribute: {} {:?}", attr.name(), attr.value()),
            }
        }
    }

    let mut iter = node.children();
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            gimli::DW_TAG_formal_parameter => {
                function.parameters.push(parse_parameter(dwarf, dwarf_unit, child)?);
            }
            tag => {
                debug!("unknown subroutine child tag: {}", tag);
            }
        }
    }
    Ok(function)
}

fn parse_unspecified_type<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    dwarf: &DwarfFileState<'input, Endian>,
    _dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    namespace: &Option<Rc<Namespace<'input>>>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<UnspecifiedType<'input>>
where
    Endian: gimli::Endianity,
{
    let mut ty = UnspecifiedType::default();
    ty.namespace = namespace.clone();

    {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_name => {
                    ty.name = attr.string_value(&dwarf.debug_str).map(|s| s.buf());
                }
                _ => {
                    debug!("unknown unspecified type attribute: {} {:?}", attr.name(), attr.value())
                }
            }
        }
    }

    let mut iter = node.children();
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            tag => {
                debug!("unknown unspecified type child tag: {}", tag);
            }
        }
    }
    Ok(ty)
}

fn parse_pointer_to_member_type<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    _dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    _namespace: &Option<Rc<Namespace<'input>>>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<PointerToMemberType>
where
    Endian: gimli::Endianity,
{
    let mut ty = PointerToMemberType::default();

    {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_type => if let Some(offset) = parse_type_offset(dwarf_unit, &attr) {
                    ty.ty = Some(offset);
                },
                gimli::DW_AT_containing_type => {
                    if let Some(offset) = parse_type_offset(dwarf_unit, &attr) {
                        ty.containing_ty = Some(offset);
                    }
                }
                gimli::DW_AT_byte_size => {
                    ty.byte_size = attr.udata_value();
                }
                _ => debug!(
                    "unknown ptr_to_member type attribute: {} {:?}",
                    attr.name(),
                    attr.value()
                ),
            }
        }
    }

    let mut iter = node.children();
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            tag => {
                debug!("unknown ptr_to_member type child tag: {}", tag);
            }
        }
    }
    Ok(ty)
}

fn parse_subprogram<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    unit: &mut Unit<'input>,
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    namespace: &Option<Rc<Namespace<'input>>>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<()>
where
    Endian: gimli::Endianity,
{
    let offset = node.entry().offset();
    let mut function = Function {
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
        inlined_functions: Vec::new(),
        variables: Vec::new(),
    };

    let mut specification = None;

    {
        let entry = node.entry();
        let mut attrs = entry.attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_name => {
                    function.name = attr.string_value(&dwarf.debug_str).map(|s| s.buf());
                }
                gimli::DW_AT_linkage_name | gimli::DW_AT_MIPS_linkage_name => {
                    function.linkage_name = attr.string_value(&dwarf.debug_str).map(|s| s.buf());
                }
                gimli::DW_AT_inline => if let gimli::AttributeValue::Inline(val) = attr.value() {
                    match val {
                        gimli::DW_INL_inlined | gimli::DW_INL_declared_inlined => {
                            function.inline = true
                        }
                        _ => function.inline = false,
                    }
                },
                gimli::DW_AT_low_pc => if let gimli::AttributeValue::Addr(addr) = attr.value() {
                    // TODO: is address 0 ever valid?
                    if addr != 0 {
                        function.low_pc = Some(addr);
                    }
                },
                gimli::DW_AT_high_pc => match attr.value() {
                    gimli::AttributeValue::Addr(addr) => function.high_pc = Some(addr),
                    gimli::AttributeValue::Udata(size) => function.size = Some(size),
                    _ => {}
                },
                gimli::DW_AT_type => if let Some(offset) = parse_type_offset(dwarf_unit, &attr) {
                    function.return_type = Some(offset);
                },
                gimli::DW_AT_specification | gimli::DW_AT_abstract_origin => {
                    if let gimli::AttributeValue::UnitRef(offset) = attr.value() {
                        let offset = offset.to_debug_info_offset(dwarf_unit.header);
                        specification = Some(offset);
                    } else {
                        debug!("unknown subprogram attribute: {} {:?}", attr.name(), attr.value())
                    }
                }
                gimli::DW_AT_declaration => {
                    if let gimli::AttributeValue::Flag(flag) = attr.value() {
                        function.declaration = flag;
                    }
                }
                gimli::DW_AT_decl_file |
                gimli::DW_AT_decl_line |
                gimli::DW_AT_frame_base |
                gimli::DW_AT_external |
                gimli::DW_AT_GNU_all_call_sites |
                gimli::DW_AT_GNU_all_tail_call_sites |
                gimli::DW_AT_prototyped |
                gimli::DW_AT_accessibility |
                gimli::DW_AT_explicit |
                gimli::DW_AT_artificial |
                gimli::DW_AT_object_pointer |
                gimli::DW_AT_virtuality |
                gimli::DW_AT_vtable_elem_location |
                gimli::DW_AT_containing_type |
                gimli::DW_AT_main_subprogram |
                gimli::DW_AT_APPLE_optimized |
                gimli::DW_AT_APPLE_omit_frame_ptr |
                gimli::DW_AT_sibling => {}
                _ => debug!("unknown subprogram attribute: {} {:?}", attr.name(), attr.value()),
            }
        }

        if let Some(low_pc) = function.low_pc {
            if let Some(high_pc) = function.high_pc {
                function.size = high_pc.checked_sub(low_pc);
            } else if let Some(size) = function.size {
                function.high_pc = low_pc.checked_add(size);
            }
        }
    }

    if let Some(specification) = specification {
        if !parse_subprogram_specification(unit, &mut function, specification) {
            dwarf_unit.subprograms.push(DwarfSubprogram {
                offset: offset,
                specification: specification,
                function: function,
            });
            return Ok(());
        }
    }

    parse_subprogram_children(unit, dwarf, dwarf_unit, &mut function, node.children())?;
    let offset = offset.to_debug_info_offset(dwarf_unit.header);
    unit.functions.insert(offset.into(), function);
    Ok(())
}

fn parse_subprogram_specification<'input>(
    unit: &mut Unit<'input>,
    function: &mut Function<'input>,
    specification: gimli::DebugInfoOffset,
) -> bool {
    let specification = match unit.functions.get(&specification.into()) {
        Some(val) => val,
        None => return false,
    };

    function.namespace = specification.namespace.clone();
    if function.name.is_none() {
        function.name = specification.name;
    }
    if function.linkage_name.is_none() {
        function.linkage_name = specification.linkage_name;
    }
    if function.return_type.is_none() {
        function.return_type = specification.return_type;
    }
    if specification.inline {
        function.inline = true;
    }
    // TODO: parameters?

    true
}

fn parse_subprogram_children<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    unit: &mut Unit<'input>,
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    function: &mut Function<'input>,
    mut iter: gimli::EntriesTreeIter<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<()>
where
    Endian: gimli::Endianity,
{
    let namespace =
        Some(Namespace::new(&function.namespace, function.name, NamespaceKind::Function));
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            gimli::DW_TAG_formal_parameter => {
                function.parameters.push(parse_parameter(dwarf, dwarf_unit, child)?);
            }
            gimli::DW_TAG_inlined_subroutine => {
                function
                    .inlined_functions
                    .push(parse_inlined_subroutine(unit, dwarf, dwarf_unit, child)?);
            }
            gimli::DW_TAG_variable => {
                let variable = parse_variable(unit, dwarf, dwarf_unit, None, child)?;
                if variable.specification.is_some() {
                    debug!(
                        "specification on subprogram declaration at offset 0x{:x}",
                        variable.offset.0
                    );
                }
                function.variables.push(variable.variable);
            }
            gimli::DW_TAG_lexical_block => {
                parse_lexical_block(
                    &mut function.inlined_functions,
                    &mut function.variables,
                    unit,
                    dwarf,
                    dwarf_unit,
                    &namespace,
                    child,
                )?;
            }
            gimli::DW_TAG_unspecified_parameters |
            gimli::DW_TAG_template_type_parameter |
            gimli::DW_TAG_template_value_parameter |
            gimli::DW_TAG_GNU_template_parameter_pack |
            gimli::DW_TAG_label |
            gimli::DW_TAG_imported_declaration |
            gimli::DW_TAG_imported_module |
            gimli::DW_TAG_GNU_call_site => {}
            tag => {
                let offset = child.entry().offset();
                let offset = offset.to_debug_info_offset(dwarf_unit.header);
                if let Some(ty) = parse_type(unit, dwarf, dwarf_unit, &namespace, child)? {
                    unit.types.insert(offset.into(), ty);
                } else {
                    debug!("unknown subprogram child tag: {}", tag);
                }
            }
        }
    }
    Ok(())
}

fn parse_parameter<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<Parameter<'input>>
where
    Endian: gimli::Endianity,
{
    let mut parameter = Parameter::default();

    {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_name => {
                    parameter.name = attr.string_value(&dwarf.debug_str).map(|s| s.buf());
                }
                gimli::DW_AT_type => if let Some(offset) = parse_type_offset(dwarf_unit, &attr) {
                    parameter.ty = Some(offset);
                },
                gimli::DW_AT_decl_file |
                gimli::DW_AT_decl_line |
                gimli::DW_AT_location |
                gimli::DW_AT_abstract_origin |
                gimli::DW_AT_artificial |
                gimli::DW_AT_const_value |
                gimli::DW_AT_sibling => {}
                _ => debug!("unknown parameter attribute: {} {:?}", attr.name(), attr.value()),
            }
        }
    }

    let mut iter = node.children();
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            tag => {
                debug!("unknown parameter child tag: {}", tag);
            }
        }
    }
    Ok(parameter)
}

fn parse_lexical_block<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    inlined_functions: &mut Vec<InlinedFunction<'input>>,
    variables: &mut Vec<Variable<'input>>,
    unit: &mut Unit<'input>,
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    namespace: &Option<Rc<Namespace<'input>>>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<()>
where
    Endian: gimli::Endianity,
{
    {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_low_pc |
                gimli::DW_AT_high_pc |
                gimli::DW_AT_ranges |
                gimli::DW_AT_sibling => {}
                _ => debug!("unknown lexical_block attribute: {} {:?}", attr.name(), attr.value()),
            }
        }
    }

    let mut iter = node.children();
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            gimli::DW_TAG_inlined_subroutine => {
                inlined_functions.push(parse_inlined_subroutine(unit, dwarf, dwarf_unit, child)?);
            }
            gimli::DW_TAG_variable => {
                let variable = parse_variable(unit, dwarf, dwarf_unit, None, child)?;
                if variable.specification.is_some() {
                    debug!(
                        "specification on subprogram declaration at offset 0x{:x}",
                        variable.offset.0
                    );
                }
                variables.push(variable.variable);
            }
            gimli::DW_TAG_lexical_block => {
                parse_lexical_block(
                    inlined_functions,
                    variables,
                    unit,
                    dwarf,
                    dwarf_unit,
                    namespace,
                    child,
                )?;
            }
            gimli::DW_TAG_formal_parameter |
            gimli::DW_TAG_label |
            gimli::DW_TAG_imported_declaration |
            gimli::DW_TAG_imported_module |
            gimli::DW_TAG_GNU_call_site => {}
            tag => {
                let offset = child.entry().offset();
                let offset = offset.to_debug_info_offset(dwarf_unit.header);
                if let Some(ty) = parse_type(unit, dwarf, dwarf_unit, namespace, child)? {
                    unit.types.insert(offset.into(), ty);
                } else {
                    debug!("unknown lexical_block child tag: {}", tag);
                }
            }
        }
    }
    Ok(())
}

fn parse_inlined_subroutine<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    unit: &mut Unit<'input>,
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<InlinedFunction<'input>>
where
    Endian: gimli::Endianity,
{
    let mut function = InlinedFunction::default();
    let mut low_pc = None;
    let mut high_pc = None;
    let mut size = None;
    let mut ranges = None;
    {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_abstract_origin => {
                    if let gimli::AttributeValue::UnitRef(offset) = attr.value() {
                        let offset = offset.to_debug_info_offset(dwarf_unit.header);
                        function.abstract_origin = Some(offset.into());
                    } else {
                        debug!(
                            "unknown inlined_subroutine attribute: {} {:?}",
                            attr.name(),
                            attr.value()
                        )
                    }
                }
                gimli::DW_AT_low_pc => if let gimli::AttributeValue::Addr(addr) = attr.value() {
                    low_pc = Some(addr);
                },
                gimli::DW_AT_high_pc => match attr.value() {
                    gimli::AttributeValue::Addr(addr) => high_pc = Some(addr),
                    gimli::AttributeValue::Udata(val) => size = Some(val),
                    _ => {}
                },
                gimli::DW_AT_ranges => {
                    if let gimli::AttributeValue::DebugRangesRef(val) = attr.value() {
                        ranges = Some(val);
                    }
                }
                gimli::DW_AT_call_file |
                gimli::DW_AT_call_line |
                gimli::DW_AT_entry_pc |
                gimli::DW_AT_sibling => {}
                _ => debug!(
                    "unknown inlined_subroutine attribute: {} {:?}",
                    attr.name(),
                    attr.value()
                ),
            }
        }
    }

    if let Some(offset) = ranges {
        let mut size = 0;
        let low_pc = low_pc.unwrap_or(0);
        let mut ranges =
            dwarf.debug_ranges.ranges(offset, dwarf_unit.header.address_size(), low_pc)?;
        while let Some(range) = ranges.next()? {
            size += range.end.wrapping_sub(range.begin);
        }
        function.size = Some(size);
    } else if let Some(size) = size {
        function.size = Some(size);
    } else if let (Some(low_pc), Some(high_pc)) = (low_pc, high_pc) {
        function.size = Some(high_pc.wrapping_sub(low_pc));
    } else {
        debug!("unknown inlined_subroutine size");
    }

    // TODO: get the namespace from the abstract origin.
    // However, not sure if this if ever actually used in practice.
    let namespace = None;

    let mut iter = node.children();
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            gimli::DW_TAG_inlined_subroutine => {
                function
                    .inlined_functions
                    .push(parse_inlined_subroutine(unit, dwarf, dwarf_unit, child)?);
            }
            gimli::DW_TAG_variable => {
                let variable = parse_variable(unit, dwarf, dwarf_unit, None, child)?;
                function.variables.push(variable.variable);
            }
            gimli::DW_TAG_lexical_block => {
                parse_lexical_block(
                    &mut function.inlined_functions,
                    &mut function.variables,
                    unit,
                    dwarf,
                    dwarf_unit,
                    &namespace,
                    child,
                )?;
            }
            gimli::DW_TAG_formal_parameter | gimli::DW_TAG_GNU_call_site => {}
            tag => {
                debug!("unknown inlined_subroutine child tag: {}", tag);
            }
        }
    }
    Ok(function)
}

fn parse_variable<'state, 'input, 'abbrev, 'unit, 'tree, Endian>(
    _unit: &mut Unit<'input>,
    dwarf: &DwarfFileState<'input, Endian>,
    dwarf_unit: &mut DwarfUnitState<'state, 'input, Endian>,
    namespace: Option<Rc<Namespace<'input>>>,
    node: gimli::EntriesTreeNode<'abbrev, 'unit, 'tree, gimli::EndianBuf<'input, Endian>>,
) -> Result<DwarfVariable<'input>>
where
    Endian: gimli::Endianity,
{
    let offset = node.entry().offset();
    let mut specification = None;
    let mut variable = Variable::default();
    variable.namespace = namespace;

    {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next()? {
            match attr.name() {
                gimli::DW_AT_name => {
                    variable.name = attr.string_value(&dwarf.debug_str).map(|s| s.buf());
                }
                gimli::DW_AT_linkage_name | gimli::DW_AT_MIPS_linkage_name => {
                    variable.linkage_name = attr.string_value(&dwarf.debug_str).map(|s| s.buf());
                }
                gimli::DW_AT_type => if let Some(offset) = parse_type_offset(dwarf_unit, &attr) {
                    variable.ty = Some(offset);
                },
                gimli::DW_AT_specification => {
                    if let gimli::AttributeValue::UnitRef(offset) = attr.value() {
                        let offset = offset.to_debug_info_offset(dwarf_unit.header);
                        specification = Some(offset);
                    } else {
                        debug!("unknown variable attribute: {} {:?}", attr.name(), attr.value())
                    }
                }
                gimli::DW_AT_declaration => {
                    if let gimli::AttributeValue::Flag(flag) = attr.value() {
                        variable.declaration = flag;
                    }
                }
                gimli::DW_AT_location => {
                    match attr.value() {
                        gimli::AttributeValue::Exprloc(expr) => {
                            match evaluate(dwarf_unit.header, expr) {
                                Some(gimli::Location::Address { address }) => {
                                    // TODO: is address 0 ever valid?
                                    if address != 0 {
                                        variable.address = Some(address);
                                    }
                                }
                                Some(gimli::Location::Register { .. }) |
                                Some(gimli::Location::Scalar { .. }) |
                                None => {}
                                Some(loc) => debug!("unknown DW_AT_location result: {:?}", loc),
                            }
                        }
                        gimli::AttributeValue::DebugLocRef(..) => {
                            // TODO
                        }
                        _ => {
                            debug!("unknown DW_AT_location: {:?}", attr.value());
                        }
                    }
                }
                gimli::DW_AT_abstract_origin |
                gimli::DW_AT_artificial |
                gimli::DW_AT_const_value |
                gimli::DW_AT_external |
                gimli::DW_AT_accessibility |
                gimli::DW_AT_alignment |
                gimli::DW_AT_decl_file |
                gimli::DW_AT_decl_line => {}
                _ => debug!("unknown variable attribute: {} {:?}", attr.name(), attr.value()),
            }
        }
    }

    let mut iter = node.children();
    while let Some(child) = iter.next()? {
        match child.entry().tag() {
            tag => {
                debug!("unknown variable child tag: {}", tag);
            }
        }
    }

    Ok(DwarfVariable {
        offset: offset,
        specification: specification,
        variable: variable,
    })
}


fn evaluate<'input, Endian>(
    unit: &gimli::CompilationUnitHeader<gimli::EndianBuf<'input, Endian>>,
    expression: gimli::Expression<gimli::EndianBuf<'input, Endian>>,
) -> Option<gimli::Location<gimli::EndianBuf<'input, Endian>>>
where
    Endian: gimli::Endianity + 'input,
{
    let mut evaluation = expression.evaluation(unit.address_size(), unit.format());
    evaluation.set_initial_value(0);
    let mut result = evaluation.evaluate();
    loop {
        match result {
            Ok(gimli::EvaluationResult::Complete) => {
                let pieces = evaluation.result();
                if pieces.len() == 1 {
                    return Some(pieces[0].location);
                } else {
                    debug!("unsupported number of evaluation pieces: {}", pieces.len());
                    return None;
                }
            }
            Ok(gimli::EvaluationResult::RequiresTextBase) => {
                result = evaluation.resume_with_text_base(0);
            }
            Ok(_) => {
                //debug!("incomplete evaluation");
                return None;
            }
            Err(e) => {
                debug!("evaluation failed: {}", e);
                return None;
            }
        }
    }
}
