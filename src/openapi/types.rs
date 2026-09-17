//! OpenAPI 3.1 AST types and data structures.
//!
//! Provides lightweight, standard OpenAPI 3.1.0 data structures with
//! deterministic serialization (BTreeMap) and full JSON Schema 2020-12 dialect compatibility.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// OpenAPI 3.1.0 root document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenApiDoc {
    pub openapi: String,
    pub info: Info,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub paths: BTreeMap<String, PathItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub components: Option<Components>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<Vec<BTreeMap<String, Vec<String>>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<Tag>>,
}

/// Metadata about the API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Info {
    pub title: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Tag metadata for grouping operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tag {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Path item defining operations available on a specific path.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PathItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub get: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub put: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Vec<ParameterOrRef>>,
}

/// An individual operation on a path.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Vec<ParameterOrRef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body: Option<RequestBodyOrRef>,
    pub responses: BTreeMap<String, ResponseItemOrRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<Vec<BTreeMap<String, Vec<String>>>>,
}

/// A parameter or a reference to a parameter in components.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ParameterOrRef {
    Ref {
        #[serde(rename = "$ref")]
        reference: String,
    },
    Item(Parameter),
}

/// Parameter definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Parameter {
    pub name: String,
    #[serde(rename = "in")]
    pub in_: ParameterIn,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
}

/// Location of a parameter.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ParameterIn {
    Query,
    Header,
    Path,
    Cookie,
}

/// Request body or reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum RequestBodyOrRef {
    Ref {
        #[serde(rename = "$ref")]
        reference: String,
    },
    Item(RequestBody),
}

/// Request body definition.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RequestBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub content: BTreeMap<String, MediaType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

/// Media type object.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MediaType {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
}

/// Response item or reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ResponseItemOrRef {
    Ref {
        #[serde(rename = "$ref")]
        reference: String,
    },
    Item(ResponseItem),
}

/// Response definition.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResponseItem {
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<BTreeMap<String, MediaType>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, HeaderOrRef>>,
}

/// Header item or reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum HeaderOrRef {
    Ref {
        #[serde(rename = "$ref")]
        reference: String,
    },
    Item(Header),
}

/// Header definition.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Header {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
}

/// Components object holding reusable schemas, parameters, security schemes, and responses.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Components {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub schemas: BTreeMap<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub responses: Option<BTreeMap<String, ResponseItemOrRef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<BTreeMap<String, Parameter>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security_schemes: Option<BTreeMap<String, SecurityScheme>>,
}

/// Security scheme definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SecurityScheme {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bearer_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Discriminator object for polymorphism in OpenAPI 3.1.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Discriminator {
    pub property_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapping: Option<BTreeMap<String, String>>,
}
