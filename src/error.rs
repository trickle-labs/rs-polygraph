/// Unified error type for all `polygraph` operations.
#[derive(thiserror::Error, Debug)]
pub enum PolygraphError {
    /// A syntax or structural error encountered while parsing an input query.
    #[error("Parse error at {span}: {message}")]
    Parse { span: String, message: String },

    /// The query uses a language feature not yet supported by this library.
    #[error("Unsupported feature: {feature}")]
    UnsupportedFeature { feature: String },

    /// The query uses a construct that is semantically infeasible to transpile
    /// to static SPARQL 1.1. Unlike `UnsupportedFeature` (which covers gaps the
    /// transpiler could eventually close), this variant marks hard limits
    /// documented in `plans/fundamental-limitations.md`.
    ///
    /// `spec_ref` is a citation of the openCypher / GQL spec section that
    /// defines the semantics, so callers can report a meaningful error.
    #[error("Unsupported construct '{construct}' ({spec_ref}): {reason}")]
    Unsupported {
        construct: String,
        spec_ref: String,
        reason: String,
    },

    /// An error occurred during translation from the AST to SPARQL algebra.
    #[error("Translation error: {message}")]
    Translation { message: String },

    /// An error occurred while mapping SPARQL results back to Cypher values.
    #[error("Result mapping error: {message}")]
    ResultMapping { message: String },
}

impl From<opencypher_parser::ParseError> for PolygraphError {
    fn from(e: opencypher_parser::ParseError) -> Self {
        match e {
            opencypher_parser::ParseError::Syntax { span, message } => {
                PolygraphError::Parse { span, message }
            }
            opencypher_parser::ParseError::UnsupportedFeature { feature } => {
                PolygraphError::UnsupportedFeature { feature }
            }
        }
    }
}
