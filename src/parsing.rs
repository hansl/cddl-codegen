use cddl::ast::*;
use either::{Either};
use std::collections::{BTreeMap};
use cddl::token::Value;

use crate::intermediate::{
    AliasIdent,
    CDDLIdent,
    EnumVariant,
    FixedValue,
    GenericDef,
    GenericInstance,
    IntermediateTypes,
    Primitive,
    Representation,
    RustIdent,
    RustRecord,
    RustStruct,
    RustType,
    RustField,
    VariantIdent,
};
use crate::utils::{
    append_number_if_duplicate,
    convert_to_camel_case,
    convert_to_snake_case,
    is_identifier_user_defined,
};

pub fn parse_rule(types: &mut IntermediateTypes, cddl_rule: &cddl::ast::Rule) {
    match cddl_rule {
        cddl::ast::Rule::Type{ rule, .. } => {
            // (1) is_type_choice_alternate ignored since shelley.cddl doesn't need it
            //     It's used, but used for no reason as it is the initial definition
            //     (which is also valid cddl), but it would be fine as = instead of /=
            // (2) ignores control operators - only used in shelley spec to limit string length for application metadata
            let rust_ident = RustIdent::new(CDDLIdent::new(rule.name.to_string()));
            let generic_params = rule
                .generic_param
                .as_ref()
                .map(|gp| gp.params.iter().map(|id| RustIdent::new(CDDLIdent::new(id.to_string()))).collect::<Vec<_>>());
            if rule.value.type_choices.len() == 1 {
                let choice = &rule.value.type_choices.first().unwrap();
                parse_type(types, &rust_ident, choice, None, generic_params);
            } else {
                parse_type_choices(types, &rust_ident, &rule.value.type_choices, None, generic_params);
            }
        },
        cddl::ast::Rule::Group{ rule, .. } => {
            assert_eq!(rule.generic_param, None, "{}: Generics not supported on plain groups", rule.name);
            // Freely defined group - no need to generate anything outside of group module
            match &rule.entry {
                cddl::ast::GroupEntry::InlineGroup{ .. } => (),// already handled in main.rs
                x => panic!("Group rule with non-inline group? {:?}", x),
            }
        },
    }
}

fn parse_type_choices(types: &mut IntermediateTypes, name: &RustIdent, type_choices: &Vec<Type1>, tag: Option<usize>, generic_params: Option<Vec<RustIdent>>) {
    let optional_inner_type = if type_choices.len() == 2 {
        let a = &type_choices[0].type2;
        let b = &type_choices[1].type2;
        if type2_is_null(a) {
            Some(b)
        } else if type2_is_null(b) {
            Some(a)
        } else {
            None
        }
    } else {
        None
    };
    if let Some(inner_type2) = optional_inner_type {
        if generic_params.is_some() {
            // the current generic support relies on having a RustStruct to swap out the types with
            // but that won't happen witn untagged T / null types since we generate an alias instead
            todo!("support foo<T> = T / null");
        }
        let inner_rust_type = rust_type_from_type2(types, inner_type2);
        match tag {
            // only want to create a wrapper if we NEED to - so that we can keep the tag information
            Some(_) => {
                types.register_rust_struct(RustStruct::new_wrapper(name.clone(), tag, RustType::Optional(Box::new(inner_rust_type)), None));
            },
            // otherwise a simple typedef of type $name = Option<$inner_rust_type>; works better
            None => {
                types.register_type_alias(name.clone(), RustType::Optional(Box::new(inner_rust_type)), true);
            },
        };
    } else {
        let variants = create_variants_from_type_choices(types, type_choices);
        let rust_struct = RustStruct::new_type_choice(name.clone(), tag, variants);
        match generic_params {
            Some(params) => types.register_generic_def(GenericDef::new(params, rust_struct)),
            None => types.register_rust_struct(rust_struct),
        };
    }
}

fn type2_to_number_literal(type2: &Type2) -> isize {
    match type2 {
        Type2::UintValue{ value, .. } => *value as isize,
        Type2::IntValue{ value, .. } => *value,
        _ => panic!("Value specified: {:?} must be a number literal to be used here", type2),
    }
}

fn parse_range_operator(range: &(RangeCtlOp, Type2)) -> (Option<isize>, Option<isize>) {
    //todo: read up on other range control operators in CDDL RFC
    match range.0 {
        RangeCtlOp::RangeOp{ .. } => panic!("Range Op only expected as 2nd type in range control operator"),
        RangeCtlOp::CtlOp{ ctrl, .. } => match ctrl {
            ".default" |
            ".cbor" |
            ".cborseq" |
            ".within" |
            ".and" => todo!("control operator {} not supported", ctrl),
            ".eq" => (Some(type2_to_number_literal(&range.1)), Some(type2_to_number_literal(&range.1))),
            // TODO: this would be MUCH nicer (for error displaying, etc) to handle this in its own dedicated way
            //       which might be necessary once we support other control operators anyway
            ".ne" => (Some(type2_to_number_literal(&range.1) + 1), Some(type2_to_number_literal(&range.1) - 1)),
            ".le" => (None, Some(type2_to_number_literal(&range.1))),
            ".lt" => (None, Some(type2_to_number_literal(&range.1) - 1)),
            ".ge" => (Some(type2_to_number_literal(&range.1)), None),
            ".gt" => (Some(type2_to_number_literal(&range.1) + 1), None),
            ".size" => match &range.1 {
                Type2::UintValue{ value, .. } => (Some(*value as isize), Some(*value as isize)),
                Type2::IntValue{ value, .. } => (Some(*value), Some(*value)),
                Type2::ParenthesizedType{ pt, .. } => {
                    assert_eq!(pt.type_choices.len(), 1);
                    let inner_type = pt.type_choices.first().unwrap();
                    let min = match inner_type.type2 {
                        Type2::UintValue{ value, .. } => Some(value as isize),
                        Type2::IntValue{ value, .. } => Some(value),
                        _ => unimplemented!("unsupoorted type in range control operator: {:?}", range),
                    };
                    let max = match &inner_type.operator {
                        Some((inner_range, inner_type_operator_t2)) => match inner_range {
                            RangeCtlOp::RangeOp{ is_inclusive, ..} => {
                                let value = match inner_type_operator_t2 {
                                    Type2::UintValue{ value, .. } => *value as isize,
                                    Type2::IntValue{ value, ..} => *value,
                                    _ => unimplemented!("unsupoorted type in range control operator: {:?}", range),
                                };
                                Some(if *is_inclusive { value } else { value + 1 })
                            },
                            RangeCtlOp::CtlOp{ .. } => panic!(""),
                        },
                        None => min,
                    };
                    (min, max)
                },
                _ => unimplemented!("unsupoorted type in range control operator: {:?}", range),
            },
            _ => panic!("Unknown (not seen in RFC-8610) range control operator: {}", ctrl),
        }
    }
}

fn parse_type(types: &mut IntermediateTypes, type_name: &RustIdent, type1: &Type1, outer_tag: Option<usize>, generic_params: Option<Vec<RustIdent>>) {
    let min_max = type1.operator.as_ref().map(parse_range_operator);
    match &type1.type2 {
        Type2::Typename{ ident, generic_arg, .. } => {
            // Note: this handles bool constants too, since we apply the type aliases and they resolve
            // and there's no Type2::BooleanValue
            let cddl_ident = CDDLIdent::new(ident.to_string());
            if min_max.is_some() {
                assert!(generic_params.is_none(), "Generics combined with range specifiers not supported");
                let field_type = RustType::Primitive(Primitive::Bytes);
                types.register_rust_struct(RustStruct::new_wrapper(type_name.clone(), outer_tag, field_type.clone(), min_max));
                types.register_type_alias(type_name.clone(), field_type, false);
            } else {
                let concrete_type = match types.new_type(&cddl_ident) {
                    RustType::Alias(_ident, ty) => *ty,
                    ty => ty,
                };
                match &generic_params {
                    Some(_params) => {
                        // this should be the only situation where you need this as otherwise the params would be unbound
                        todo!("generics on defined types e.g. foo<T, U> = [T, U], bar<V> = foo<V, uint>");
                        // TODO: maybe you could do this by resolving it here then storing the resolved one as GenericDef
                    },
                    None => {
                        match generic_arg {
                            Some(arg) => {
                                // This is for named generic instances such as:
                                // foo = bar<text>
                                let generic_args = arg.args.iter().map(|a| rust_type_from_type2(types, &a.type2)).collect();
                                types.register_generic_instance(GenericInstance::new(type_name.clone(), RustIdent::new(cddl_ident.clone()), generic_args))
                            },
                            None => {
                                types.register_type_alias(type_name.clone(), concrete_type, true);
                            }
                        }
                    }
                }
            }
        },
        Type2::Map{ group, .. } => {
            parse_group(types, group, type_name, Representation::Map, outer_tag, generic_params);
        },
        Type2::Array{ group, .. } => {
            // TODO: We could potentially generate an array-wrapper type around this
            // possibly based on the occurency specifier.
            parse_group(types, group, type_name, Representation::Array, outer_tag, generic_params);
        },
        Type2::TaggedData{ tag, t, .. } => {
            if let Some(_) = outer_tag {
                panic!("doubly nested tags are not supported");
            }
            tag.expect("not sure what empty tag here would mean - unsupported");
            match t.type_choices.len() {
                1 => {
                    let inner_type = &t.type_choices.first().unwrap();
                    match match &inner_type.type2 {
                        Type2::Typename{ ident, .. } => Either::Right(ident),
                        Type2::Map{ group, .. } => Either::Left(group),
                        Type2::Array{ group, .. } => Either::Left(group),
                        x => panic!("only supports tagged arrays/maps/typenames - found: {:?} in rule {}", x, type_name),
                    } {
                        Either::Left(_group) => parse_type(types, type_name, inner_type, *tag, generic_params),
                        Either::Right(ident) => {
                            let new_type = types.new_type(&CDDLIdent::new(ident.to_string()));
                            types.register_rust_struct(RustStruct::new_wrapper(type_name.clone(), *tag, new_type, min_max))
                        },
                    };
                },
                _ => {
                    parse_type_choices(types, type_name, &t.type_choices, *tag, generic_params);
                }
            };
        },
        // Note: bool constants are handled via Type2::Typename
        Type2::IntValue{ value, .. } => {
            types.register_type_alias(type_name.clone(), RustType::Fixed(FixedValue::Int(*value)), true);
        },
        Type2::UintValue{ value, .. } => {
            types.register_type_alias(type_name.clone(), RustType::Fixed(FixedValue::Uint(*value)), true);
        },
        Type2::TextValue{ value, .. } => {
            types.register_type_alias(type_name.clone(), RustType::Fixed(FixedValue::Text(value.clone())), true);
        },
        x => {
            panic!("\nignored typename {} -> {:?}\n", type_name, x);
        },
    }
}

// TODO: Also generates individual choices if required, ie for a / [foo] / c would generate Foos
pub fn create_variants_from_type_choices(types: &mut IntermediateTypes, type_choices: &Vec<Type1>) -> Vec<EnumVariant> {
    let mut variant_names_used = BTreeMap::<String, u32>::new();
    type_choices.iter().map(|type1| {
        let rust_type = rust_type_from_type2(types, &type1.type2);
        let variant_name = append_number_if_duplicate(&mut variant_names_used, rust_type.for_variant().to_string());
        EnumVariant::new(VariantIdent::new_custom(variant_name), rust_type, false)
    }).collect()
}

fn table_domain_range(group_choice: &GroupChoice, rep: Representation) -> Option<(&Type2, &Type)> {
    // Here we test if this is a struct vs a table.
    // struct: { x: int, y: int }, etc
    // table: { * int => tstr }, etc
    // this assumes that all maps representing tables are homogenous
    // and contain no other fields. I am not sure if this is a guarantee in
    // cbor but I would hope that the cddl specs we are using follow this.
    if let Representation::Map = rep {
        if group_choice.group_entries.len() == 1 {
            match group_choice.group_entries.first() {
                Some((GroupEntry::ValueMemberKey{ ge, .. }, _)) => {
                    match &ge.member_key {
                        Some(MemberKey::Type1{ t1, .. }) => {
                            // TODO: Do we need to handle cuts for what we're doing?
                            // Does the range control operator matter?
                            return Some((&t1.type2, &ge.entry_type));
                        },
                        _ => panic!("unsupported table map key (1): {:?}", ge),
                    }
                },
                _ => panic!("unsupported table map key (2): {:?}", group_choice.group_entries.first().unwrap()),
            }
        }
    }
    // Could not get a table-type domain/range - must be a heterogenous struct
    None
}

// would use rust_type_from_type2 but that requires IntermediateTypes which we shouldn't
fn type2_is_null(t2: &Type2) -> bool {
    match t2 {
        Type2::Typename{ ident, .. } => ident.ident == "null" || ident.ident == "nil",
        _ => false,
    }
}

fn type_to_field_name(t: &Type) -> Option<String> {
    let type2_to_field_name = |t2: &Type2| match t2 {
        Type2::Typename{ ident, .. } => Some(ident.to_string()),
            Type2::TextValue { value, .. } => Some(value.clone()),
            Type2::Array { group, .. } => match group.group_choices.len() {
                1 => {
                    let entries = &group.group_choices.first().unwrap().group_entries;
                    match entries.len() {
                        1 => {
                            match &entries.first().unwrap().0 {
                                // should we do this? here it possibly allows [[foo]] -> fooss
                                GroupEntry::ValueMemberKey{ ge, .. } => Some(format!("{}s", type_to_field_name(&ge.entry_type)?)),
                                GroupEntry::TypeGroupname{ ge, .. } => Some(format!("{}s", ge.name.to_string())),
                                GroupEntry::InlineGroup { .. } => None,
                            }
                        },
                        // only supports homogenous arrays for now
                        _ => None,
                    }
                },
                // no group choice support here
                _ => None
            }
            // non array/text/identifier types not supported here - value keys are caught earlier anyway
            _ => None
    };
    match t.type_choices.len() {
        1 => type2_to_field_name(&t.type_choices.first().unwrap().type2),
        2 => {
            // special case for T / null -> maps to Option<T> so field name should be same as just T
            let a = &t.type_choices[0].type2;
            let b = &t.type_choices[1].type2;
            if type2_is_null(a) {
                type2_to_field_name(b)
            } else if type2_is_null(b) {
                type2_to_field_name(a)
            } else {
                // neither are null - we do not support type choices here
                None
            }
        },
        // no type choice support here
        _ => None,
    }
}

// Attempts to use the style-converted type name as a field name, and if we have already
// generated one, then we simply add numerals starting at 2, 3, 4...
// If you wish to only check if there is an explicitly stated field name,
// then use group_entry_to_raw_field_name()
fn group_entry_to_field_name(entry: &GroupEntry, index: usize, already_generated: &mut BTreeMap<String, u32>) -> String {
    let field_name = convert_to_snake_case(&match entry {
        GroupEntry::ValueMemberKey{ ge, .. } => match ge.member_key.as_ref() {
            Some(member_key) => match member_key {
                MemberKey::Value{ value, .. } => {
                    let value = match value {
                        Value::INT(i) => format!("_{}", i),
                        Value::UINT(u) =>  format!("_{}", u),
                        Value::FLOAT(f) =>  format!("_{}", f),
                        Value::TEXT(txt) => txt.to_string(),
                        Value::BYTE(b) => b.to_string(),
                    };

                    value.replace(|c: char| c.is_ascii_alphanumeric(), "_")
                },
                MemberKey::Bareword{ ident, .. } => ident.to_string(),
                MemberKey::Type1{ t1, .. } => match t1.type2 {
                    Type2::UintValue{ value, .. } => format!("key_{}", value),
                    _ => panic!("Encountered Type1 member key in multi-field map - not supported: {:?}", entry),
                },
                _ => unreachable!()
            },
            None => {
                type_to_field_name(&ge.entry_type).unwrap_or_else(|| format!("index_{}", index))
            }
        },
        GroupEntry::TypeGroupname{ ge: TypeGroupnameEntry { name, .. }, .. } => match !is_identifier_user_defined(&name.to_string()) {
            true => format!("index_{}", index),
            false => name.to_string(),
        },
        GroupEntry::InlineGroup{ group, .. } => panic!("not implemented (define a new struct for this!) = {}\n\n {:?}", group, group),
    });
    append_number_if_duplicate(already_generated, field_name.clone())
}

// Only returns Some(String) if there was an explicit field name provided, otherwise None.
// If you need to try and make one using the type/etc, then try group_entry_to_field_name()
// Also does not do any CamelCase or snake_case formatting.
fn group_entry_to_raw_field_name(entry: &GroupEntry) -> Option<String> {
    match entry {
        GroupEntry::ValueMemberKey{ ge, .. } => match ge.member_key.as_ref() {
            Some(MemberKey::Bareword{ ident, .. } ) => Some(ident.to_string()),
            _ => None,
        },
        GroupEntry::TypeGroupname{ ge: TypeGroupnameEntry { name, .. }, .. } => match !is_identifier_user_defined(&name.to_string()) {
            true => None,
            false => Some(name.to_string()),
        },
        GroupEntry::InlineGroup{ group, .. } => panic!("not implemented (define a new struct for this!) = {}\n\n {:?}", group, group),
    }
}

fn rust_type_from_type2(types: &mut IntermediateTypes, type2: &Type2) -> RustType {
    // TODO: socket plugs (used in hash type)
    match type2 {
        Type2::UintValue{ value, .. } => RustType::Fixed(FixedValue::Uint(*value)),
        Type2::IntValue{ value, .. } => RustType::Fixed(FixedValue::Int(*value)),
        //Type2::FloatValue{ value, .. } => RustType::Fixed(FixedValue::Float(*value)),
        Type2::TextValue{ value, .. } => RustType::Fixed(FixedValue::Text(value.clone())),
        Type2::Typename{ ident, generic_arg, .. } => {
            let cddl_ident = CDDLIdent::new(&ident.ident);
            match generic_arg {
                Some(args) => {
                    // This is for anonymous instances (i.e. members) such as:
                    // foo = [a: bar<text, bool>]
                    // so to be able to expose it to wasm, we create a new generic instance
                    // under the name bar_string_bool in this case.
                    let generic_args = args.args.iter().map(|a| rust_type_from_type2(types, &a.type2)).collect::<Vec<_>>();
                    let args_name = generic_args.iter().map(|t| t.for_variant().to_string()).collect::<Vec<String>>().join("_");
                    let instance_cddl_ident = CDDLIdent::new(format!("{}_{}", cddl_ident, args_name));
                    let instance_ident = RustIdent::new(instance_cddl_ident.clone());
                    let generic_ident = RustIdent::new(cddl_ident);
                    types.register_generic_instance(GenericInstance::new(instance_ident, generic_ident, generic_args));
                    types.new_type(&instance_cddl_ident)
                },
                None => types.new_type(&cddl_ident),
            }
        },
        Type2::Array{ group, .. } => {
            // TODO: support for group choices in arrays?
            let element_type = match group.group_choices.len() {
                1 => {
                    let choice = &group.group_choices.first().unwrap();
                    // special case for homogenous arrays
                    if choice.group_entries.len() == 1 {
                        let (entry, _has_comma) = choice.group_entries.first().unwrap();
                        match entry {
                            GroupEntry::ValueMemberKey{ ge, .. } => rust_type(types, &ge.entry_type),
                            GroupEntry::TypeGroupname{ ge, .. } => types.new_type(&CDDLIdent::new(&ge.name.to_string())),
                            _ => panic!("UNSUPPORTED_ARRAY_ELEMENT<{:?}>", entry),
                        }
                    } else {
                        // array of non-choice element that has multiple fields: tuples? create seperately?
                        panic!("TODO: how do we handle this? tuples? or creating a struct definition and referring to it by name?")
                    }
                },
                // array of elements with choices: enums?
                _ => unimplemented!("group choices in array type not supported"),
            };
            let array_wrapper_name = element_type.name_as_array();
            types.create_and_register_array_type(element_type, &array_wrapper_name)
        },
        Type2::Map { group, .. } => {
            match group.group_choices.len() {
                1 => {
                    let group_choice = group.group_choices.first().unwrap();
                    let table_types = table_domain_range(group_choice, Representation::Map);
                    match table_types {
                        // Table map - homogenous key/value types
                        Some((domain, range)) => {
                            let key_type = rust_type_from_type2(types, domain);
                            let value_type = rust_type(types, range);
                            // Generate a MapTToV for a { t => v } table-type map as we are an anonymous type
                            // defined as part of another type if we're in this level of parsing.
                            // We also can't have plain groups unlike arrays, so don't try and generate those
                            // for general map types we can though but not for tables
                            let table_type_ident = RustIdent::new(CDDLIdent::new(format!("Map{}To{}", key_type.for_member(), value_type.for_member())));
                            types.register_rust_struct(RustStruct::new_table(table_type_ident, None, key_type.clone(), value_type.clone()));
                            RustType::Map(Box::new(key_type), Box::new(value_type))
                        },
                        None => unimplemented!("TODO: non-table types as types: {:?}", group),
                    }
                },
                _ => unimplemented!("group choices in inlined map types not allowed: {:?}", group),
            }
        },
        // unsure if we need to handle the None case - when does this happen?
        Type2::TaggedData{ tag, t, .. } => {
            RustType::Tagged(tag.expect("tagged data without tag not supported"), Box::new(rust_type(types, t)))
        },
        _ => {
            panic!("Ignoring Type2: {:?}", type2);
        },
    }
}

fn rust_type(types: &mut IntermediateTypes, t: &Type) -> RustType {
    if t.type_choices.len() == 1 {
        rust_type_from_type2(types, &t.type_choices.first().unwrap().type2)
    } else {
        if t.type_choices.len() == 2 {
            // T / null   or   null / T   should map to Option<T>
            let a = &t.type_choices[0].type2;
            let b = &t.type_choices[1].type2;
            if type2_is_null(a) {
                return RustType::Optional(Box::new(rust_type_from_type2(types, b)));
            }
            if type2_is_null(b) {
                return RustType::Optional(Box::new(rust_type_from_type2(types, a)));
            }
        }
        let variants = create_variants_from_type_choices(types, &t.type_choices);
        let mut combined_name = String::new();
        // one caveat: nested types can leave ambiguous names and cause problems like
        // (a / b) / c and a / (b / c) would both be AOrBOrC
        for variant in &variants {
            if !combined_name.is_empty() {
                combined_name.push_str("Or");
            }
            // due to undercase primitive names, we need to convert here
            combined_name.push_str(&variant.rust_type.for_variant().to_string());
        }
        let combined_ident = RustIdent::new(CDDLIdent::new(&combined_name));
        types.register_rust_struct(RustStruct::new_type_choice(combined_ident, None, variants));
        types.new_type(&CDDLIdent::new(combined_name))
    }
}

fn group_entry_optional(entry: &GroupEntry) -> bool {
    match match entry {
        GroupEntry::ValueMemberKey{ ge, .. } => &ge.occur,
        GroupEntry::TypeGroupname{ ge, .. } => &ge.occur,
        GroupEntry::InlineGroup{ .. } => panic!("inline group entries are not implemented"),
    } {
        Some(Occur::Optional(_)) => true,
        _ => false,
    }
}

fn group_entry_to_type(types: &mut IntermediateTypes, entry: &GroupEntry) -> RustType {
    let ret = match entry {
        GroupEntry::ValueMemberKey{ ge, .. } => rust_type(types, &ge.entry_type),
        GroupEntry::TypeGroupname{ ge, .. } => {
            if ge.generic_arg.is_some() {
                // I am not sure how we end up with this kind of generic args since definitional ones
                // and member ones are created elsewhere. I thought that if you had a field like
                // foo: bar<uint> it would be here but it turns out it's in the ValueMemberKey
                // variant instead.
                todo!("If you run into this please create a github issue and include the .cddl that caused it");
            }
            let cddl_ident = CDDLIdent::new(ge.name.to_string());
            types.new_type(&cddl_ident)
        },
        GroupEntry::InlineGroup{ .. } => panic!("inline group entries are not implemented"),
    };
    //println!("group_entry_to_typename({:?}) = {:?}\n", entry, ret);
    ret
}

fn group_entry_to_key(entry: &GroupEntry) -> Option<FixedValue> {
    match entry {
        GroupEntry::ValueMemberKey{ ge, .. } => {
            match ge.member_key.as_ref()? {
                MemberKey::Value{ value, .. } => match value {
                    cddl::token::Value::UINT(x) => Some(FixedValue::Uint(*x)),
                    cddl::token::Value::INT(x) => Some(FixedValue::Int(*x)),
                    cddl::token::Value::TEXT(x) => Some(FixedValue::Text(x.clone())),
                    _ => panic!("unsupported map identifier(1): {:?}", value),
                },
                MemberKey::Bareword{ ident, .. } => Some(FixedValue::Text(ident.to_string())),
                MemberKey::Type1{ t1, .. } => match &t1.type2 {
                    Type2::UintValue{ value, .. } => Some(FixedValue::Uint(*value)),
                    Type2::IntValue{ value, .. } => Some(FixedValue::Int(*value)),
                    Type2::TextValue{ value, .. } => Some(FixedValue::Text(value.clone())),
                    _ => panic!("unsupported map identifier(2): {:?}", entry),
                },
                _ => unreachable!()
            }
        },
        _ => None,
    }
}

fn parse_record_from_group_choice(types: &mut IntermediateTypes, rep: Representation, group_choice: &GroupChoice) -> RustRecord {
    let mut generated_fields = BTreeMap::<String, u32>::new();
    let fields = group_choice.group_entries.iter().enumerate().map(
        |(index, (group_entry, _has_comma))| {
            let field_name = group_entry_to_field_name(group_entry, index, &mut generated_fields);
            // does not exist for fixed values importantly
            let field_type = group_entry_to_type(types, group_entry);
            if let RustType::Rust(ident) = &field_type {
                types.set_rep_if_plain_group(ident, rep);
            }
            let optional_field = group_entry_optional(group_entry);
            let key = match rep {
                Representation::Map => Some(group_entry_to_key(group_entry).expect("map fields need keys")),
                Representation::Array => None,
            };
            RustField::new(field_name, field_type, optional_field, key)
        }
    ).collect();
    RustRecord {
        rep,
        fields,
    }
}

fn parse_group_choice(types: &mut IntermediateTypes, group_choice: &GroupChoice, name: &RustIdent, rep: Representation, tag: Option<usize>, generic_params: Option<Vec<RustIdent>>) {
    let table_types = table_domain_range(group_choice, rep);
    let rust_struct = match table_types {
        // Table map - homogenous key/value types
        Some((domain, range)) => {
            let key_type = rust_type_from_type2(types, domain);
            let value_type = rust_type(types, range);
            RustStruct::new_table(name.clone(), tag, key_type, value_type)
        },
        // Heterogenous map (or array!) with defined key/value pairs in the cddl like a struct
        None => {
            let record = parse_record_from_group_choice(types, rep, group_choice);
            // We need to store this in IntermediateTypes so we can refer from one struct to another.
            RustStruct::new_record(name.clone(), tag, record)
        }
    };
    match generic_params {
        Some(params) => types.register_generic_def(GenericDef::new(params, rust_struct)),
        None => types.register_rust_struct(rust_struct),
    };
}

pub fn parse_group(types: &mut IntermediateTypes, group: &Group, name: &RustIdent, rep: Representation, tag: Option<usize>, generic_params: Option<Vec<RustIdent>>) {
    if group.group_choices.len() == 1 {
        // Handle simple (no choices) group.
        parse_group_choice(types, group.group_choices.first().unwrap(), name, rep, tag, generic_params);
    } else {
        if generic_params.is_some() {
            todo!("{}: generic group choices not supported", name);
        }
        // Generate Enum object that is not exposed to wasm, since wasm can't expose
        // fully featured rust enums via wasm_bindgen

        // TODO: We don't support generating SerializeEmbeddedGroup for group choices which is necessary for plain groups
        // It would not be as trivial to add as we do the outer group's array/map tag writing inside the variant match
        // to avoid having to always generate SerializeEmbeddedGroup when not necessary.
        assert!(!types.is_plain_group(name));

        // Handle group with choices by generating an enum then generating a group for every choice
        let mut variants_names_used = BTreeMap::<String, u32>::new();
        let variants: Vec<EnumVariant> = group.group_choices.iter().enumerate().map(|(i, group_choice)| {
            // If we're a 1-element we should just wrap that type in the variant rather than
            // define a new struct just for each variant.
            // TODO: handle map-based enums? It would require being able to extract the key logic
            // We might end up doing this anyway to support table-maps in choices though.
            if group_choice.group_entries.len() == 1 {
                let group_entry = &group_choice.group_entries.first().unwrap().0;
                let ty = group_entry_to_type(types, group_entry);
                let serialize_as_embedded = if let RustType::Rust(ident) = &ty {
                    // we might need to generate it if not used elsewhere
                    types.set_rep_if_plain_group(ident, rep);
                    types.is_plain_group(ident)
                } else {
                    false
                };
                let variant_ident = convert_to_camel_case(&match group_entry_to_raw_field_name(group_entry) {
                    Some(name) => name,
                    None => append_number_if_duplicate(&mut variants_names_used, ty.for_variant().to_string()),
                });
                let variant_ident = VariantIdent::new_custom(variant_ident);
                EnumVariant::new(variant_ident, ty, serialize_as_embedded)
                // None => {
                //     // TODO: Weird case, group choice with only one fixed-value field.
                //     // What should we do here? In the future we could make this a
                //     // non-value-taking enum then handle this in the serialization code.
                //     // However, for now we just default to default behavior:
                //     let variant_name = format!("{}{}", name, i);
                //     // TODO: Should we generate these within their own namespace?
                //     codegen_group_choice(global, group_choice, &variant_name, rep, None);
                //     EnumVariant::new(variant_name.clone(), RustType::Rust(variant_name), true)
                // },
            } else {
                // General case, GroupN type identifiers and generate group choice since it's inlined here
                let variant_name = RustIdent::new(CDDLIdent::new(format!("{}{}", name, i)));
                types.mark_plain_group(variant_name.clone(), None);
                // TODO: Should we generate these within their own namespace?
                parse_group_choice(types, group_choice, &variant_name, rep, None, generic_params.clone());
                EnumVariant::new(VariantIdent::new_rust(variant_name.clone()), RustType::Rust(variant_name), true)
            }
        }).collect();
        types.register_rust_struct(RustStruct::new_group_choice(name.clone(), tag, variants, rep));
    }
}
