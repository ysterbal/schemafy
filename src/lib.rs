// This would be nice once it stabilizes:
// https://github.com/rust-lang/rust/issues/44732
// #![feature(external_doc)]
// #![doc(include = "../README.md")]

//! This is a Rust crate which can take a [json schema (draft
//! 4)](http://json-schema.org/) and generate Rust types which are
//! serializable with [serde](https://serde.rs/). No checking such as
//! `min_value` are done but instead only the structure of the schema
//! is followed as closely as possible.
//!
//! As a schema could be arbitrarily complex this crate makes no
//! guarantee that it can generate good types or even any types at all
//! for a given schema but the crate does manage to bootstrap itself
//! which is kind of cool.
//!
//! ## Example
//!
//! Generated types for VS Codes [debug server protocol][]: <https://docs.rs/debugserver-types>
//!
//! [debug server protocol]:https://code.visualstudio.com/docs/extensions/example-debuggers
//!
//! ## Usage
//!
//! Rust types can be generated by passing a path to a JSON schema to the [`schemafy`]
//! procedural macro.
//!
//! ```rust
//! extern crate serde;
//! extern crate schemafy_core;
//! extern crate serde_json;
//!
//! use serde::{Serialize, Deserialize};
//!
//! schemafy::schemafy!(
//!     "tests/nested.json"
//! );
//!
//! schemafy::schemafy!(
//!     root: Schema // Optional name for the root type (if one exists)
//!     "src/schema.json"
//! );
//!
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let nested: Defnested = serde_json::from_str(r#"{ "append": "abc" }"#)?;
//!     assert_eq!(nested.append, Some("abc".to_string()));
//!     Ok(())
//! }
//! ```
use schemafy_core;

#[macro_use]
extern crate serde_derive;
use serde_json;

use syn;
#[macro_use]
extern crate quote;
extern crate proc_macro;

/// Types from the JSON Schema meta-schema (draft 4).
///
/// This module is itself generated from a JSON schema.
mod schema;

use std::borrow::Cow;

use inflector::Inflector;

use serde_json::Value;

use crate::schema::{Schema, SimpleTypes};

use proc_macro2::{Span, TokenStream};

fn replace_invalid_identifier_chars(s: &str) -> String {
    s.replace(|c: char| !c.is_alphanumeric() && c != '_', "_")
}

fn rename_keyword(prefix: &str, s: &str) -> Option<Tokens> {
    let keywords = [
        "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
        "return", "self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use",
        "where", "while", "abstract", "become", "box", "do", "final", "macro", "override", "priv",
        "typeof", "unsized", "virtual", "yield", "async", "await", "try",
    ];
    if keywords.iter().any(|&keyword| keyword == s) {
        let n = syn::Ident::new(&format!("{}_", s), Span::call_site());
        let prefix = syn::Ident::new(prefix, Span::call_site());
        Some(quote! {
            #[serde(rename = #s)]
            #prefix #n
        })
    } else {
        None
    }
}

fn field(s: &str) -> TokenStream {
    if let Some(t) = rename_keyword("pub", s) {
        t
    } else {
        let snake = s.to_snake_case();
        if snake != s || snake.contains(|c: char| c == '$' || c == '#') {
            let field = if snake == "ref" {
                syn::Ident::new("ref_".into(), Span::call_site())
            } else {
                syn::Ident::new(&snake.replace('$', "").replace('#', ""), Span::call_site())
            };

            quote! {
                #[serde(rename = #s)]
                pub #field
            }
        } else {
            let field = syn::Ident::new(s, Span::call_site());
            quote!( pub #field )
        }
    }
}

fn merge_option<T, F>(mut result: &mut Option<T>, r: &Option<T>, f: F)
where
    F: FnOnce(&mut T, &T),
    T: Clone,
{
    *result = match (&mut result, r) {
        (&mut &mut Some(ref mut result), &Some(ref r)) => return f(result, r),
        (&mut &mut None, &Some(ref r)) => Some(r.clone()),
        _ => return (),
    };
}

fn merge_all_of(result: &mut Schema, r: &Schema) {
    use std::collections::btree_map::Entry;

    for (k, v) in &r.properties {
        match result.properties.entry(k.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(v.clone());
            }
            Entry::Occupied(mut entry) => merge_all_of(entry.get_mut(), v),
        }
    }

    if let Some(ref ref_) = r.ref_ {
        result.ref_ = Some(ref_.clone());
    }

    if let Some(ref description) = r.description {
        result.description = Some(description.clone());
    }

    merge_option(&mut result.required, &r.required, |required, r_required| {
        required.extend(r_required.iter().cloned());
    });

    result.type_.retain(|e| r.type_.contains(e));
}

const LINE_LENGTH: usize = 100;
const INDENT_LENGTH: usize = 4;

fn make_doc_comment(mut comment: &str, remaining_line: usize) -> TokenStream {
    let mut out_comment = String::new();
    out_comment.push_str("/// ");
    let mut length = 4;
    while let Some(word) = comment.split(char::is_whitespace).next() {
        if comment.is_empty() {
            break;
        }
        comment = &comment[word.len()..];
        if length + word.len() >= remaining_line {
            out_comment.push_str("\n/// ");
            length = 4;
        }
        out_comment.push_str(word);
        length += word.len();
        let mut n = comment.chars();
        match n.next() {
            Some('\n') => {
                out_comment.push_str("\n");
                out_comment.push_str("/// ");
                length = 4;
            }
            Some(_) => {
                out_comment.push_str(" ");
                length += 1;
            }
            None => (),
        }
        comment = n.as_str();
    }
    if out_comment.ends_with(' ') {
        out_comment.pop();
    }
    out_comment.push_str("\n");
    out_comment.parse().unwrap()
}

struct FieldExpander<'a, 'r: 'a> {
    default: bool,
    expander: &'a mut Expander<'r>,
}

impl<'a, 'r> FieldExpander<'a, 'r> {
    fn expand_fields(&mut self, type_name: &str, schema: &Schema) -> Vec<TokenStream> {
        let schema = self.expander.schema(schema);
        schema
            .properties
            .iter()
            .map(|(field_name, value)| {
                self.expander.current_field.clone_from(field_name);
                let key = field(field_name);
                let required = schema
                    .required
                    .iter()
                    .flat_map(|a| a.iter())
                    .any(|req| req == field_name);
                let field_type = self.expander.expand_type(type_name, required, value);
                if !field_type.typ.starts_with("Option<") {
                    self.default = false;
                }
                let typ = field_type.typ.parse::<TokenStream>().unwrap();

                let default = if field_type.default {
                    Some(quote! { #[serde(default)] })
                } else {
                    None
                };
                let attributes = if field_type.attributes.is_empty() {
                    None
                } else {
                    let attributes = field_type
                        .attributes
                        .iter()
                        .map(|attr| attr.parse::<TokenStream>().unwrap());
                    Some(quote! {
                        #[serde( #(#attributes),* )]
                    })
                };
                let comment = value
                    .description
                    .as_ref()
                    .map(|comment| make_doc_comment(comment, LINE_LENGTH - INDENT_LENGTH));
                quote! {
                    #comment
                    #default
                    #attributes
                    #key : #typ
                }
            })
            .collect()
    }
}

struct Expander<'r> {
    root_name: Option<&'r str>,
    schemafy_path: &'r str,
    root: &'r Schema,
    current_type: String,
    current_field: String,
    types: Vec<(String, TokenStream)>,
}

struct FieldType {
    typ: String,
    attributes: Vec<String>,
    default: bool,
}

impl<S> From<S> for FieldType
where
    S: Into<String>,
{
    fn from(s: S) -> FieldType {
        FieldType {
            typ: s.into(),
            attributes: Vec::new(),
            default: false,
        }
    }
}

impl<'r> Expander<'r> {
    fn new(root_name: Option<&'r str>, schemafy_path: &'r str, root: &'r Schema) -> Expander<'r> {
        Expander {
            root_name,
            root,
            schemafy_path,
            current_field: "".into(),
            current_type: "".into(),
            types: Vec::new(),
        }
    }

    fn type_ref(&self, s: &str) -> String {
        let s = if s == "#" {
            self.root_name.expect("No root name specified for schema")
        } else {
            s.split('/').last().expect("Component")
        };
        replace_invalid_identifier_chars(&s.to_pascal_case())
    }

    fn schema(&self, schema: &'r Schema) -> Cow<'r, Schema> {
        let schema = match schema.ref_ {
            Some(ref ref_) => self.schema_ref(ref_),
            None => schema,
        };
        match schema.all_of {
            Some(ref all_of) if !all_of.is_empty() => {
                all_of
                    .iter()
                    .skip(1)
                    .fold(self.schema(&all_of[0]).clone(), |mut result, def| {
                        merge_all_of(result.to_mut(), &self.schema(def));
                        result
                    })
            }
            _ => Cow::Borrowed(schema),
        }
    }

    fn schema_ref(&self, s: &str) -> &'r Schema {
        s.split('/').fold(self.root, |schema, comp| {
            if comp == "#" {
                self.root
            } else if comp == "definitions" {
                schema
            } else {
                schema
                    .definitions
                    .get(comp)
                    .unwrap_or_else(|| panic!("Expected definition: `{}` {}", s, comp))
            }
        })
    }

    fn expand_type(&mut self, type_name: &str, required: bool, typ: &Schema) -> FieldType {
        let mut result = self.expand_type_(typ);
        if type_name == result.typ {
            result.typ = format!("Box<{}>", result.typ)
        }
        if !required && !result.default {
            result.typ = format!("Option<{}>", result.typ)
        }
        result
    }

    fn expand_type_(&mut self, typ: &Schema) -> FieldType {
        if let Some(ref ref_) = typ.ref_ {
            self.type_ref(ref_).into()
        } else if typ.any_of.as_ref().map_or(false, |a| a.len() == 2) {
            let any_of = typ.any_of.as_ref().unwrap();
            let simple = self.schema(&any_of[0]);
            let array = self.schema(&any_of[1]);
            if !array.type_.is_empty() {
                if let SimpleTypes::Array = array.type_[0] {
                    if simple == self.schema(&array.items[0]) {
                        return FieldType {
                            typ: format!("Vec<{}>", self.expand_type_(&any_of[0]).typ),
                            attributes: vec![format!(
                                r#"with="{}one_or_many""#,
                                self.schemafy_path
                            )],
                            default: true,
                        };
                    }
                }
            }
            return "serde_json::Value".into();
        } else if typ.type_.len() == 2 {
            if typ.type_[0] == SimpleTypes::Null || typ.type_[1] == SimpleTypes::Null {
                let mut ty = typ.clone();
                ty.type_.retain(|x| *x != SimpleTypes::Null);

                FieldType {
                    typ: format!("Option<{}>", self.expand_type_(&ty).typ),
                    attributes: vec![],
                    default: true,
                }
            } else {
                "serde_json::Value".into()
            }
        } else if typ.type_.len() == 1 {
            match typ.type_[0] {
                SimpleTypes::String => {
                    if typ.enum_.as_ref().map_or(false, |e| e.is_empty()) {
                        "serde_json::Value".into()
                    } else {
                        "String".into()
                    }
                }
                SimpleTypes::Integer => "i64".into(),
                SimpleTypes::Boolean => "bool".into(),
                SimpleTypes::Number => "f64".into(),
                // Handle objects defined inline
                SimpleTypes::Object
                    if !typ.properties.is_empty()
                        || typ.additional_properties == Some(Value::Bool(false)) =>
                {
                    let name = format!(
                        "{}{}",
                        self.current_type.to_pascal_case(),
                        self.current_field.to_pascal_case()
                    );
                    let tokens = self.expand_schema(&name, typ);
                    self.types.push((name.clone(), tokens));
                    name.into()
                }
                SimpleTypes::Object => {
                    let prop = match typ.additional_properties {
                        Some(ref props) if props.is_object() => {
                            let prop = serde_json::from_value(props.clone()).unwrap();
                            self.expand_type_(&prop).typ
                        }
                        _ => "serde_json::Value".into(),
                    };
                    let result = format!("::std::collections::BTreeMap<String, {}>", prop);
                    FieldType {
                        typ: result,
                        attributes: Vec::new(),
                        default: typ.default == Some(Value::Object(Default::default())),
                    }
                }
                SimpleTypes::Array => {
                    let item_type = typ.items.get(0).map_or("serde_json::Value".into(), |item| {
                        self.current_type = format!("{}Item", self.current_type);
                        self.expand_type_(item).typ
                    });
                    format!("Vec<{}>", item_type).into()
                }
                _ => "serde_json::Value".into(),
            }
        } else {
            "serde_json::Value".into()
        }
    }

    pub fn expand_definitions(&mut self, schema: &Schema) {
        for (name, def) in &schema.definitions {
            let type_decl = self.expand_schema(name, def);
            let definition_tokens = match def.description {
                Some(ref comment) => {
                    let t = make_doc_comment(comment, LINE_LENGTH);
                    quote! {
                        #t
                        #type_decl
                    }
                }
                None => type_decl,
            };
            self.types.push((name.to_string(), definition_tokens));
        }
    }

    pub fn expand_schema(&mut self, original_name: &str, schema: &Schema) -> TokenStream {
        self.expand_definitions(schema);

        let pascal_case_name = replace_invalid_identifier_chars(&original_name.to_pascal_case());
        self.current_type.clone_from(&pascal_case_name);
        let (fields, default) = {
            let mut field_expander = FieldExpander {
                default: true,
                expander: self,
            };
            let fields = field_expander.expand_fields(original_name, schema);
            (fields, field_expander.default)
        };
        let name = syn::Ident::new(&pascal_case_name, Span::call_site());
        let is_struct =
            !fields.is_empty() || schema.additional_properties == Some(Value::Bool(false));
        let type_decl = if is_struct {
            if default {
                quote! {
                    #[derive(Clone, PartialEq, Debug, Default, Deserialize, Serialize)]
                    pub struct #name {
                        #(#fields),*
                    }
                }
            } else {
                quote! {
                    #[derive(Clone, PartialEq, Debug, Deserialize, Serialize)]
                    pub struct #name {
                        #(#fields),*
                    }
                }
            }
        } else if schema.enum_.as_ref().map_or(false, |e| !e.is_empty()) {
            let mut optional = false;
            let variants = schema
                .enum_
                .as_ref()
                .map_or(&[][..], |v| v)
                .iter()
                .flat_map(|v| match *v {
                    Value::String(ref v) => {
                        let pascal_case_variant = v.to_pascal_case();
                        let variant_name =
                            rename_keyword("", &pascal_case_variant).unwrap_or_else(|| {
                                let v = syn::Ident::new(&pascal_case_variant, Span::call_site());
                                quote!(#v)
                            });
                        Some(if pascal_case_variant == *v {
                            variant_name
                        } else {
                            quote! {
                                #[serde(rename = #v)]
                                #variant_name
                            }
                        })
                    }
                    Value::Null => {
                        optional = true;
                        None
                    }
                    _ => panic!("Expected string for enum got `{}`", v),
                })
                .collect::<Vec<_>>();

            if optional {
                let enum_name = syn::Ident::new(&format!("{}_", name), Span::call_site());
                quote! {
                    pub type #name = Option<#enum_name>;
                    #[derive(Clone, PartialEq, Debug, Deserialize, Serialize)]
                    pub enum #enum_name {
                        #(#variants),*
                    }
                }
            } else {
                quote! {
                    #[derive(Clone, PartialEq, Debug, Deserialize, Serialize)]
                    pub enum #name {
                        #(#variants),*
                    }
                }
            }
        } else {
            let typ = self
                .expand_type("", true, schema)
                .typ
                .parse::<TokenStream>()
                .unwrap();
            return quote! {
                pub type #name = #typ;
            };
        };
        if name == original_name {
            type_decl
        } else {
            quote! {
                #[serde(rename = #original_name)]
                #type_decl
            }
        }
    }

    pub fn expand(&mut self, schema: &Schema) -> TokenStream {
        match self.root_name {
            Some(name) => {
                let schema = self.expand_schema(name, schema);
                self.types.push((name.to_string(), schema));
            }
            None => self.expand_definitions(schema),
        }

        let types = self.types.iter().map(|t| &t.1);

        quote! {
            #( #types )*
        }
    }
}

impl<'a> Default for GenerateBuilder<'a> {
    fn default() -> Self {
        GenerateBuilder {
            root_name: None,
            schemafy_path: "::schemafy_core::",
        }
    }
}

/// A configurable builder for generating Rust types from a JSON
/// schema.
///
/// The default options are usually fine. In that case, you can use
/// the [`generate()`](fn.generate.html) convenience method instead.
struct GenerateBuilder<'a> {
    /// The name of the root type defined by the schema. If the schema
    /// does not define a root type (some schemas are simply a
    /// collection of definitions) then simply pass `None`.
    pub root_name: Option<String>,
    /// The module path to this crate. Some generated code may make
    /// use of types defined in this crate. Unless you have
    /// re-exported this crate or imported it under a different name,
    /// the default should be fine.
    pub schemafy_path: &'a str,
}

impl<'a> GenerateBuilder<'a> {
    fn build_tokens(mut self, tokens: proc_macro::TokenStream) -> proc_macro::TokenStream {
        struct Def {
            root: Option<String>,
            input_file: syn::LitStr,
        }

        impl syn::parse::Parse for Def {
            fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
                let root = if input.peek(syn::Ident) {
                    let root_ident: syn::Ident = input.parse()?;
                    if root_ident != "root" {
                        return Err(syn::Error::new(root_ident.span(), "Expected `root`"));
                    }
                    input.parse::<syn::Token![:]>()?;
                    Some(input.parse::<syn::Ident>()?.to_string())
                } else {
                    None
                };
                Ok(Def {
                    root,
                    input_file: input.parse()?,
                })
            }
        }

        let def = syn::parse_macro_input!(tokens as Def);
        self.root_name = def.root;

        let input_file = def.input_file.value();
        let json = std::fs::read_to_string(&input_file)
            .unwrap_or_else(|err| panic!("Unable to read `{}`: {}", input_file, err));

        let schema = serde_json::from_str(&json).unwrap_or_else(|err| panic!("{}", err));
        let mut expander = Expander::new(
            self.root_name.as_ref().map(|s| &**s),
            self.schemafy_path,
            &schema,
        );
        expander.expand(&schema).into()
    }
}

/// Generate Rust types from a JSON schema.
///
/// If the `root` parameter is supplied, then a type will be
/// generated from the root of the schema.
///
/// ```rust
/// extern crate serde;
/// extern crate schemafy_core;
/// extern crate serde_json;
///
/// use serde::{Serialize, Deserialize};
///
/// schemafy::schemafy!(
///     root: MyRoot // Optional name for the root type (if one exists)
///     "tests/nested.json"
/// );
///
/// fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let nested: Defnested = serde_json::from_str(r#"{ "append": "abc" }"#)?;
///     assert_eq!(nested.append, Some("abc".to_string()));
///     Ok(())
/// }
/// ```
#[proc_macro]
pub fn schemafy(tokens: proc_macro::TokenStream) -> proc_macro::TokenStream {
    GenerateBuilder {
        ..GenerateBuilder::default()
    }
    .build_tokens(tokens.into())
    .into()
}

#[doc(hidden)]
#[proc_macro]
pub fn regenerate(tokens: proc_macro::TokenStream) -> proc_macro::TokenStream {
    use std::process::Command;

    let tokens = GenerateBuilder {
        ..GenerateBuilder::default()
    }
    .build_tokens(tokens);

    {
        let out = tokens.to_string();
        std::fs::write("src/schema.rs", &out).unwrap();
        Command::new("rustfmt")
            .arg("src/schema.rs")
            .output()
            .unwrap();
    }

    tokens
}
