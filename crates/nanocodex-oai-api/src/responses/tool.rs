//! Model-visible tool declarations and open JSON boundary values.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Model-visible tool definition carried by Responses Lite input.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolDefinition {
    /// JSON-schema function tool.
    Function {
        /// Model-visible tool name.
        name: Box<str>,
        /// Concrete guidance for when and how to use the tool.
        description: Box<str>,
        /// Whether the provider should enforce the parameter schema strictly.
        strict: bool,
        /// JSON Schema accepted as function arguments.
        parameters: JsonSchema,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// Optional JSON Schema produced by the function.
        output_schema: Option<JsonSchema>,
    },
    /// Free-form custom tool constrained by a grammar.
    Custom {
        /// Model-visible tool name.
        name: Box<str>,
        /// Concrete guidance for when and how to use the tool.
        description: Box<str>,
        /// Free-form input grammar.
        format: CustomToolFormat,
    },
}

impl ToolDefinition {
    /// Creates a function tool with non-strict parameters.
    ///
    /// ```
    /// use nanocodex_oai_api::{
    ///     responses::JsonSchema,
    ///     tools::ToolDefinition,
    /// };
    /// use serde_json::json;
    ///
    /// let definition = ToolDefinition::function(
    ///     "lookup_region",
    ///     "Return deployment metadata for one exact region identifier.",
    ///     JsonSchema::from(json!({
    ///         "type": "object",
    ///         "properties": { "region": { "type": "string" } },
    ///         "required": ["region"],
    ///         "additionalProperties": false
    ///     })),
    /// );
    ///
    /// assert_eq!(definition.name(), "lookup_region");
    /// ```
    #[must_use]
    pub fn function(
        name: impl Into<Box<str>>,
        description: impl Into<Box<str>>,
        parameters: impl Into<JsonSchema>,
    ) -> Self {
        Self::Function {
            name: name.into(),
            description: description.into(),
            strict: false,
            parameters: parameters.into(),
            output_schema: None,
        }
    }

    /// Creates a custom tool with grammar-constrained free-form input.
    #[must_use]
    pub fn custom(
        name: impl Into<Box<str>>,
        description: impl Into<Box<str>>,
        format: CustomToolFormat,
    ) -> Self {
        Self::Custom {
            name: name.into(),
            description: description.into(),
            format,
        }
    }

    /// Adds an output schema to a function definition.
    ///
    /// Custom tool definitions are returned unchanged.
    #[must_use]
    pub fn with_output_schema(mut self, output_schema: impl Into<JsonSchema>) -> Self {
        if let Self::Function {
            output_schema: current,
            ..
        } = &mut self
        {
            *current = Some(output_schema.into());
        }
        self
    }

    /// Returns the model-visible tool name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Function { name, .. } | Self::Custom { name, .. } => name,
        }
    }

    /// Returns the model-visible tool description.
    #[must_use]
    pub fn description(&self) -> &str {
        match self {
            Self::Function { description, .. } | Self::Custom { description, .. } => description,
        }
    }

    /// Returns function parameters, or `None` for a custom tool.
    #[must_use]
    pub const fn parameters(&self) -> Option<&JsonSchema> {
        match self {
            Self::Function { parameters, .. } => Some(parameters),
            Self::Custom { .. } => None,
        }
    }

    /// Returns a function output schema when configured.
    #[must_use]
    pub const fn output_schema(&self) -> Option<&JsonSchema> {
        match self {
            Self::Function { output_schema, .. } => output_schema.as_ref(),
            Self::Custom { .. } => None,
        }
    }
}

/// Grammar configuration for a custom tool's free-form input.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CustomToolFormat {
    #[serde(rename = "type")]
    /// Provider format kind. Constructors currently produce `grammar`.
    pub kind: Box<str>,
    /// Grammar syntax, such as `lark`.
    pub syntax: Box<str>,
    /// Complete grammar definition.
    pub definition: Box<str>,
}

impl CustomToolFormat {
    /// Creates a grammar-constrained custom-tool format.
    #[must_use]
    pub fn grammar(syntax: impl Into<Box<str>>, definition: impl Into<Box<str>>) -> Self {
        Self {
            kind: "grammar".into(),
            syntax: syntax.into(),
            definition: definition.into(),
        }
    }
}

/// Open JSON Schema fragment used at the model/tool boundary.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(transparent)]
pub struct JsonSchema(Value);

impl JsonSchema {
    /// Borrows the underlying JSON Schema value.
    #[must_use]
    pub const fn as_value(&self) -> &Value {
        &self.0
    }
}

impl From<Value> for JsonSchema {
    fn from(value: Value) -> Self {
        Self(value)
    }
}

/// Arbitrary JSON retained for protocol fields whose schema is provider-defined.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(transparent)]
pub struct JsonValue(Value);

impl JsonValue {
    /// Borrows the retained provider-defined JSON value.
    #[must_use]
    pub const fn as_value(&self) -> &Value {
        &self.0
    }
}

impl From<Value> for JsonValue {
    fn from(value: Value) -> Self {
        Self(value)
    }
}
