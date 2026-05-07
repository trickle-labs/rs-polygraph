/// Error type for the `opencypher-parser` crate.
///
/// Only covers parse-time failures. Translation and result-mapping errors
/// live in the `polygraph` crate's `PolygraphError`.
#[derive(thiserror::Error, Debug)]
pub enum ParseError {
    /// A syntax or structural error encountered while parsing an input query.
    #[error("Parse error at {span}: {message}")]
    Syntax { span: String, message: String },

    /// The query uses a language feature not yet supported by this library.
    #[error("Unsupported feature: {feature}")]
    UnsupportedFeature { feature: String },
}
