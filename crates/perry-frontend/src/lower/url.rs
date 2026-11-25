//! Lowering for `URL` and `URLSearchParams` constructors, properties, and methods.
use super::*;

impl<'s> FunctionBuilder<'s, '_> {
    pub(super) fn url_field(
        &mut self,
        ctx: &mut Context,
        value: Value,
        field: &str,
    ) -> Result<Value> {
        if field == "searchParams" {
            return Ok(self.intrinsic(ctx, Intrinsic::UrlSearchParams, vec![value]));
        }
        let index = [
            "href", "protocol", "pathname", "username", "password", "hash", "search", "host",
            "hostname", "port", "origin",
        ]
        .iter()
        .position(|&name| name == field)
        .ok_or_else(|| unsupported!("unsupported URL field {field}"))?;
        let index = self.number(ctx, index as f64);
        Ok(self.intrinsic(ctx, Intrinsic::UrlGet, vec![value, index]))
    }

    pub(super) fn url_string(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<Value> {
        let value = self.expression(ctx, expr)?;
        let value = self.defined(ctx, value)?;
        match kind(ctx, value)? {
            Kind::String => Ok(value),
            Kind::Url => self.url_field(ctx, value, "href"),
            _ => bail_diag!(codes::TYPE_MISMATCH, "a string or URL is required here"),
        }
    }

    pub(super) fn url_expression(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
    ) -> Result<Option<Value>> {
        match expr {
            Expr::UrlGetSearchParams(url) => {
                let url = self.typed_operand(ctx, url, Kind::Url, "a URL")?;
                Ok(Some(self.intrinsic(
                    ctx,
                    Intrinsic::UrlSearchParams,
                    vec![url],
                )))
            }
            Expr::UrlNew { url, base } => {
                let input = self.url_string(ctx, url)?;
                let base = match base {
                    Some(base) if !matches!(&**base, Expr::Undefined) => {
                        let base = self.url_string(ctx, base)?;
                        self.optional(ctx, base)
                    }
                    _ => {
                        let string = StringType::get(ctx).into();
                        self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, string))
                    }
                };
                Ok(self.call_intrinsic(ctx, Intrinsic::UrlNew, vec![input, base]))
            }
            Expr::UrlSetPathname { url, value } => {
                let url = self.typed_operand(ctx, url, Kind::Url, "a URL")?;
                let value = self.string_operand(ctx, value)?;
                self.intrinsic_void(ctx, Intrinsic::UrlSetPathname, vec![url, value]);
                Ok(Some(value))
            }
            Expr::EncodeURIComponent(value) => {
                let value = self.string_operand(ctx, value)?;
                Ok(self.call_intrinsic(ctx, Intrinsic::EncodeUriComponent, vec![value]))
            }
            _ => Ok(None),
        }
    }
}
