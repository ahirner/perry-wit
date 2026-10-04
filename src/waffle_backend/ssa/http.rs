//! Statically checked buffered HTTP calls and immutable response metadata.

use super::FunctionLowerer;
use crate::waffle_backend::{abi, http};
use anyhow::{Context, Result, bail, ensure};
use perry_hir::{ir::Expr, types::Type as HirType};
use waffle::{MemoryArg, Operator, Type, Value};

impl FunctionLowerer<'_> {
    pub(super) fn http_get(&mut self, arguments: &[Expr]) -> Result<Value> {
        ensure!(
            arguments.len() == 5,
            "HTTP get requires scheme, authority, path, headers, and maximum response bytes"
        );
        let mut values = Vec::new();
        for argument in &arguments[..3] {
            values.push(self.string_receiver(argument)?);
        }
        let HirType::Object(headers) = self.infer_expr_type(&arguments[3]) else {
            bail!("HTTP request headers must be a statically typed string dictionary");
        };
        ensure!(
            headers
                .properties
                .values()
                .all(|field| field.ty == HirType::String && !field.optional)
                && headers
                    .index_signature
                    .as_deref()
                    .is_none_or(|ty| *ty == HirType::String),
            "HTTP request header values must be strings"
        );
        values.push(self.expression(&arguments[3])?);
        ensure!(
            self.infer_expr_type(&arguments[4]) == HirType::Number,
            "HTTP response limit must be a number"
        );
        values.push(self.expression(&arguments[4])?);
        let payload = self.call_completion(self.registry.http_helpers.unwrap().get, &values);
        Ok(abi::decode_payload(
            &mut self.body,
            self.block,
            payload,
            true,
        ))
    }

    pub(super) fn http_property(&mut self, receiver: &Expr, property: &str) -> Result<Value> {
        let response = self.expression(receiver)?;
        let offset = match property {
            "status" => 0,
            "headerCount" => 8,
            "body" => 12,
            _ => bail!("Unsupported HttpResponse property '{property}'"),
        };
        let value = self.op(
            Operator::I32Load {
                memory: MemoryArg {
                    align: 2,
                    offset,
                    memory: self.registry.memory,
                },
            },
            &[response],
            &[Type::I32],
        );
        Ok(if property == "body" {
            value
        } else {
            self.op(Operator::F64ConvertI32U, &[value], &[Type::F64])
        })
    }

    pub(super) fn http_header(
        &mut self,
        receiver: &Expr,
        method: &str,
        arguments: &[Expr],
    ) -> Result<Value> {
        ensure!(
            http::is_response(&self.infer_expr_type(receiver)),
            "Expected an HTTP response"
        );
        ensure!(
            matches!(method, "headerName" | "headerValue"),
            "Unsupported HttpResponse method '{method}'"
        );
        ensure!(
            arguments.len() == 1 && self.infer_expr_type(&arguments[0]) == HirType::Number,
            "HttpResponse.{method} requires one numeric index"
        );
        let response = self.expression(receiver)?;
        let index = self.expression(&arguments[0])?;
        let bytes = self.op(
            Operator::I32Const {
                value: u32::from(method == "headerValue"),
            },
            &[],
            &[Type::I32],
        );
        let payload = self.call_completion(
            self.registry
                .http_helpers
                .context("HTTP response methods require a GET operation in this module")?
                .header,
            &[response, index, bytes],
        );
        Ok(abi::decode_payload(
            &mut self.body,
            self.block,
            payload,
            true,
        ))
    }
}
