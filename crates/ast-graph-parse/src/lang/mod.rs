pub mod rust;
pub mod python;
pub mod javascript;
pub mod csharp;
pub mod java;
pub mod go;
pub mod ruby;
pub mod ruby_rails;
pub mod ruby_rails_routes;
pub mod php;
pub mod swift;

use ast_graph_core::Language;
use crate::extractor::LanguageExtractor;

/// Get the appropriate extractor for a language.
pub fn get_extractor(language: Language) -> Box<dyn LanguageExtractor> {
    match language {
        Language::Rust => Box::new(rust::RustExtractor),
        Language::Python => Box::new(python::PythonExtractor),
        Language::JavaScript | Language::TypeScript => {
            Box::new(javascript::JavaScriptExtractor::new(language))
        }
        Language::CSharp => Box::new(csharp::CSharpExtractor),
        Language::Java => Box::new(java::JavaExtractor),
        Language::Go => Box::new(go::GoExtractor),
        Language::Ruby => Box::new(ruby::RubyExtractor),
        Language::Php => Box::new(php::PhpExtractor),
        Language::Swift => Box::new(swift::SwiftExtractor),
    }
}
