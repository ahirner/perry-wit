//! Preserve parser diagnostics discarded by the upstream wrapper.
use anyhow::Result;
use perry_parser::{Spanned, parse_typescript_with_cache, swc_ecma_ast as ast};
use perry_wit_diagnostics::{DiagnosticCode, PerryDiagnostic, SourceSpan};
use swc_ecma_parser::{Parser, Syntax, TsSyntax, lexer::Lexer};

pub(crate) fn parse(source: &str, identity: &str) -> Result<ast::Module> {
    let mut cache = Default::default();
    let parsed = parse_typescript_with_cache(source, &format!("{identity}.mts"), &mut cache)
        .map_err(|error| {
            // The pinned wrapper discards fatal spans; recover SWC metadata directly.
            let lexer = Lexer::new(
                Syntax::Typescript(TsSyntax::default()),
                Default::default(),
                swc_common::input::StringInput::new(
                    source,
                    swc_common::BytePos(1),
                    swc_common::BytePos(source.len() as u32 + 1),
                ),
                None,
            );
            let mut parser = Parser::new_from(lexer);
            let mut diagnostic = PerryDiagnostic::error(
                DiagnosticCode::ParseError,
                format!("Parsing {identity}: {error}"),
            );
            if let Err(failure) = parser.parse_module() {
                let span = failure.span();
                diagnostic.span = Some(SourceSpan::new(
                    identity,
                    source,
                    span.lo.0.saturating_sub(1) as usize,
                    span.hi.0.saturating_sub(1) as usize,
                ));
            }
            error.context(diagnostic)
        })?;
    if let Some(diagnostic) = parsed.diagnostics.iter().next() {
        return Err(PerryDiagnostic::error(
            DiagnosticCode::ParseError,
            format!("Parsing {identity}: {}", diagnostic.message),
        )
        .with_span(SourceSpan::new(
            identity,
            source,
            diagnostic.span.start.saturating_sub(1) as usize,
            diagnostic.span.end.saturating_sub(1) as usize,
        ))
        .into());
    }
    Ok(parsed.module)
}
