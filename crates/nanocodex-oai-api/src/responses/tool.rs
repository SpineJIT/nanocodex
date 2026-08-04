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
        /// Whether this definition is returned for deferred provider loading.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
        /// JSON Schema accepted as function arguments.
        parameters: JsonSchema,
        #[serde(skip)]
        /// Optional JSON Schema produced by the function.
        ///
        /// This is client-owned Code Mode metadata and is not part of the
        /// Responses tool declaration sent to the model.
        output_schema: Option<JsonSchema>,
    },
    /// Responses namespace containing related function tools.
    Namespace {
        /// Model-visible namespace name.
        name: Box<str>,
        /// Guidance describing the namespace as a whole.
        description: Box<str>,
        /// Function declarations exposed under this namespace.
        tools: Vec<Self>,
    },
    /// Free-form custom tool constrained by a grammar.
    Custom {
        /// Model-visible tool name.
        name: Box<str>,
        /// Concrete guidance for when and how to use the tool.
        description: Box<str>,
        /// Whether this definition is returned for deferred provider loading.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
        /// Free-form input grammar.
        format: CustomToolFormat,
    },
    /// Provider-native deferred-tool search executed by the client.
    ToolSearch {
        /// Execution owner. Client-side handlers use `client`.
        execution: Box<str>,
        /// Concrete guidance for when and how to search deferred tools.
        description: Box<str>,
        /// JSON Schema accepted as search arguments.
        parameters: JsonSchema,
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
            defer_loading: None,
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
            defer_loading: None,
            format,
        }
    }

    /// Creates a Responses namespace from function tool declarations.
    #[must_use]
    pub fn namespace(
        name: impl Into<Box<str>>,
        description: impl Into<Box<str>>,
        tools: impl IntoIterator<Item = Self>,
    ) -> Self {
        Self::Namespace {
            name: name.into(),
            description: description.into(),
            tools: tools.into_iter().collect(),
        }
    }

    /// Creates a provider-native deferred-tool search definition.
    ///
    /// ```
    /// use nanocodex_oai_api::{
    ///     responses::JsonSchema,
    ///     tools::ToolDefinition,
    /// };
    /// use serde_json::json;
    ///
    /// let definition = ToolDefinition::tool_search(
    ///     "client",
    ///     "Search the application's deferred calendar tools.",
    ///     JsonSchema::from(json!({
    ///         "type": "object",
    ///         "properties": {
    ///             "query": { "type": "string" },
    ///             "limit": { "type": "number" }
    ///         },
    ///         "required": ["query"],
    ///         "additionalProperties": false
    ///     })),
    /// );
    ///
    /// assert_eq!(definition.name(), "tool_search");
    /// ```
    #[must_use]
    pub fn tool_search(
        execution: impl Into<Box<str>>,
        description: impl Into<Box<str>>,
        parameters: impl Into<JsonSchema>,
    ) -> Self {
        Self::ToolSearch {
            execution: execution.into(),
            description: description.into(),
            parameters: parameters.into(),
        }
    }

    /// Adds an output schema to a function definition.
    ///
    /// Custom and provider-native tool definitions are returned unchanged.
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

    /// Marks a function or custom definition for provider-native deferred loading.
    #[must_use]
    #[allow(
        clippy::missing_const_for_fn,
        reason = "ToolDefinition owns drop types that const evaluation rejects"
    )]
    pub fn with_deferred_loading(mut self) -> Self {
        match &mut self {
            Self::Function { defer_loading, .. } | Self::Custom { defer_loading, .. } => {
                *defer_loading = Some(true);
            }
            Self::Namespace { .. } | Self::ToolSearch { .. } => {}
        }
        self
    }

    /// Returns whether this function or custom definition requests deferred loading.
    #[must_use]
    pub const fn is_deferred(&self) -> bool {
        match self {
            Self::Function { defer_loading, .. } | Self::Custom { defer_loading, .. } => {
                matches!(defer_loading, Some(true))
            }
            Self::Namespace { .. } | Self::ToolSearch { .. } => false,
        }
    }

    /// Returns the model-visible tool name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Function { name, .. }
            | Self::Namespace { name, .. }
            | Self::Custom { name, .. } => name,
            Self::ToolSearch { .. } => "tool_search",
        }
    }

    /// Returns the model-visible tool description.
    #[must_use]
    pub fn description(&self) -> &str {
        match self {
            Self::Function { description, .. }
            | Self::Namespace { description, .. }
            | Self::Custom { description, .. } => description,
            Self::ToolSearch { description, .. } => description,
        }
    }

    /// Returns function or tool-search parameters, or `None` for a custom tool.
    #[must_use]
    pub const fn parameters(&self) -> Option<&JsonSchema> {
        match self {
            Self::Function { parameters, .. } | Self::ToolSearch { parameters, .. } => {
                Some(parameters)
            }
            Self::Namespace { .. } | Self::Custom { .. } => None,
        }
    }

    /// Returns a function output schema when configured.
    #[must_use]
    pub const fn output_schema(&self) -> Option<&JsonSchema> {
        match self {
            Self::Function { output_schema, .. } => output_schema.as_ref(),
            Self::Namespace { .. } | Self::Custom { .. } | Self::ToolSearch { .. } => None,
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{CustomToolFormat, JsonSchema, ToolDefinition};

    #[test]
    fn function_and_custom_tools_opt_into_deferred_loading_without_changing_eager_shapes() {
        let function = ToolDefinition::function(
            "lookup",
            "Look up a value.",
            json!({ "type": "object", "properties": {} }),
        );
        assert!(!function.is_deferred());
        assert!(
            serde_json::to_value(&function).unwrap()["defer_loading"].is_null(),
            "eager definitions must preserve their existing wire shape"
        );

        let custom = ToolDefinition::custom(
            "patch",
            "Apply a patch.",
            CustomToolFormat::grammar("lark", "start: \"patch\""),
        )
        .with_deferred_loading();
        assert!(custom.is_deferred());
        assert_eq!(
            serde_json::to_value(&custom).unwrap()["defer_loading"],
            true
        );
        let namespace = ToolDefinition::namespace("editing", "Deferred editing tools.", [custom]);
        assert_eq!(
            serde_json::to_value(namespace).unwrap()["tools"][0]["type"],
            "custom"
        );
    }

    #[test]
    fn namespace_serializes_function_children_without_client_output_metadata() {
        let definition = ToolDefinition::namespace(
            "image_gen",
            "Tools in the image_gen namespace.",
            [ToolDefinition::function(
                "imagegen",
                "Generate an image.",
                json!({
                    "type": "object",
                    "properties": {"prompt": {"type": "string"}},
                    "required": ["prompt"],
                    "additionalProperties": false
                }),
            )
            .with_output_schema(json!({"type": "object"}))],
        );

        assert_eq!(
            serde_json::to_value(definition).unwrap(),
            json!({
                "type": "namespace",
                "name": "image_gen",
                "description": "Tools in the image_gen namespace.",
                "tools": [{
                    "type": "function",
                    "name": "imagegen",
                    "description": "Generate an image.",
                    "strict": false,
                    "parameters": {
                        "type": "object",
                        "properties": {"prompt": {"type": "string"}},
                        "required": ["prompt"],
                        "additionalProperties": false
                    }
                }]
            })
        );
    }

    #[test]
    fn tool_search_serializes_the_provider_native_shape() {
        let definition = ToolDefinition::tool_search(
            "client",
            "Search caller-configured deferred tools.",
            JsonSchema::from(json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query for deferred tools."
                    },
                    "limit": {
                        "type": "number",
                        "description": "Maximum number of tools to return."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            })),
        );

        assert_eq!(definition.name(), "tool_search");
        assert_eq!(
            definition.description(),
            "Search caller-configured deferred tools."
        );
        assert_eq!(
            serde_json::to_value(definition).unwrap(),
            json!({
                "type": "tool_search",
                "execution": "client",
                "description": "Search caller-configured deferred tools.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query for deferred tools."
                        },
                        "limit": {
                            "type": "number",
                            "description": "Maximum number of tools to return."
                        }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            })
        );
    }
}
