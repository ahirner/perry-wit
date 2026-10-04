//! Statically checked buffered HTTP calls and immutable response metadata.

use super::FunctionLowerer;
use crate::waffle_backend::{abi, http};
use anyhow::{Context, Result, bail, ensure};
use perry_hir::{ir::Expr, types::Type as HirType};
use waffle::{MemoryArg, Operator, Type, Value};

impl FunctionLowerer<'_> {
    pub(super) fn http_get(&mut self, name: &str, arguments: &[Expr]) -> Result<Value> {
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
        if let Some(record) = self.start_task(
            &crate::waffle_backend::promises::TaskTarget::Intrinsic(name.into()),
            &values,
        )? {
            return Ok(record);
        }
        let payload = self.call_completion(self.registry.http_helpers.unwrap().get, &values);
        Ok(abi::decode_payload(
            &mut self.body,
            self.block,
            payload,
            true,
        ))
    }

    pub(super) fn http_property(&mut self, receiver: &Expr, property: &str) -> Result<Value> {
        if http::fetch::is_response(&self.infer_expr_type(receiver)) {
            return self.fetch_property(receiver, property);
        }
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

impl FunctionLowerer<'_> {
    pub(super) fn fetch(&mut self, name: &str, arguments: &[Expr]) -> Result<Value> {
        ensure!(
            arguments.len() == 1,
            "fetch currently requires one absolute HTTP(S) URL; Request and options lowering is not implemented yet"
        );
        let url = self.string_receiver(&arguments[0])?;
        if let Some(record) = self.start_task(
            &crate::waffle_backend::promises::TaskTarget::Intrinsic(name.into()),
            &[url],
        )? {
            return Ok(record);
        }
        let payload = self.call_completion(
            self.registry.http_helpers.unwrap().fetch.unwrap().fetch,
            &[url],
        );
        Ok(abi::decode_payload(
            &mut self.body,
            self.block,
            payload,
            true,
        ))
    }

    pub(super) fn fetch_body(
        &mut self,
        receiver: &Expr,
        method: &str,
        arguments: &[Expr],
    ) -> Result<Value> {
        let method = http::fetch::BodyMethod::named(method)
            .with_context(|| format!("Response.{method} lowering is not implemented yet"))?;
        ensure!(
            arguments.is_empty(),
            "Response body methods take no arguments"
        );
        let response = self.expression(receiver)?;
        if let Some(record) = self.start_task(
            &crate::waffle_backend::promises::TaskTarget::FetchBody(method),
            &[response],
        )? {
            return Ok(record);
        }
        let payload = self.call_completion(
            self.registry
                .http_helpers
                .unwrap()
                .fetch
                .unwrap()
                .body(method),
            &[response],
        );
        Ok(abi::decode_payload(
            &mut self.body,
            self.block,
            payload,
            true,
        ))
    }

    fn fetch_property(&mut self, receiver: &Expr, property: &str) -> Result<Value> {
        let offset = match property {
            "status" | "ok" => 0,
            "url" => 16,
            "bodyUsed" => 20,
            _ => bail!("Response.{property} lowering is not implemented yet"),
        };
        let response = self.expression(receiver)?;
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
        Ok(match property {
            "url" | "bodyUsed" => value,
            "ok" => {
                let lower = self.op(Operator::I32Const { value: 200 }, &[], &[Type::I32]);
                let upper = self.op(Operator::I32Const { value: 299 }, &[], &[Type::I32]);
                let above = self.op(Operator::I32GeU, &[value, lower], &[Type::I32]);
                let below = self.op(Operator::I32LeU, &[value, upper], &[Type::I32]);
                self.op(Operator::I32And, &[above, below], &[Type::I32])
            }
            _ => self.op(Operator::F64ConvertI32U, &[value], &[Type::F64]),
        })
    }
}
