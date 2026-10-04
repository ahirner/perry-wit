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
            !arguments.is_empty() && arguments.len() <= 2,
            "fetch requires a URL and optional typed RequestInit record"
        );
        let url = self.string_receiver(&arguments[0])?;
        let zero = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let mut values = [url, zero, zero, zero, zero];
        if let Some(options) = arguments.get(1) {
            let HirType::Object(shape) = self.infer_expr_type(options) else {
                bail!("fetch options require a statically typed RequestInit record");
            };
            for (key, field) in &shape.properties {
                ensure!(
                    matches!(key.as_str(), "method" | "headers" | "body"),
                    "fetch option '{key}' is not implemented yet"
                );
                ensure!(
                    !field.optional,
                    "fetch option '{key}' must have a statically known value"
                );
                match key.as_str() {
                    "method" => ensure!(
                        matches!(field.ty, HirType::String | HirType::StringLiteral(_)),
                        "fetch method must be a string"
                    ),
                    "headers" => {
                        let HirType::Object(headers) = &field.ty else {
                            bail!("fetch headers require a typed string record");
                        };
                        ensure!(
                            headers.properties.values().all(|field| !field.optional
                                && matches!(field.ty, HirType::String | HirType::StringLiteral(_)))
                                && headers
                                    .index_signature
                                    .as_deref()
                                    .is_none_or(|ty| *ty == HirType::String),
                            "fetch header values must be strings"
                        );
                    }
                    "body" => ensure!(
                        matches!(field.ty, HirType::String | HirType::StringLiteral(_))
                            || crate::waffle_backend::bytes::is_byte_view(&field.ty),
                        "fetch body must be string or Uint8Array"
                    ),
                    _ => unreachable!(),
                }
            }
            let object = self.expression(options)?;
            let helpers = self.registry.object_helpers.unwrap();
            for (name, index) in [("method", 1), ("headers", 2), ("body", 3)] {
                if let Some(field) = shape.properties.get(name) {
                    let key = self.expression(&Expr::String(name.into()))?;
                    let entry = self.op(
                        Operator::Call {
                            function_index: helpers.get,
                        },
                        &[object, key],
                        &[Type::I32],
                    );
                    let tag = crate::waffle_backend::values::ValueTag::of(&field.ty)? as u32;
                    let tag = self.op(Operator::I32Const { value: tag }, &[], &[Type::I32]);
                    let payload = self.call_completion(helpers.value, &[entry, tag, zero]);
                    values[index] = abi::decode_payload(&mut self.body, self.block, payload, true);
                    if name == "body" {
                        let kind = if crate::waffle_backend::bytes::is_byte_view(&field.ty) {
                            2
                        } else {
                            1
                        };
                        values[4] = self.op(Operator::I32Const { value: kind }, &[], &[Type::I32]);
                    }
                }
            }
        }
        if let Some(record) = self.start_task(
            &crate::waffle_backend::promises::TaskTarget::Intrinsic(name.into()),
            &values,
        )? {
            return Ok(record);
        }
        let payload = self.call_completion(
            self.registry.http_helpers.unwrap().fetch.unwrap().fetch,
            &values,
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

    pub(super) fn fetch_header(
        &mut self,
        receiver: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<Value> {
        ensure!(
            matches!(method, "get" | "has"),
            "Headers.{method} lowering is not implemented yet"
        );
        ensure!(args.len() == 1, "Headers.{method} requires one string name");
        let headers = self.expression(receiver)?;
        let name = self.string_receiver(&args[0])?;
        let query = self.op(
            Operator::I32Const {
                value: u32::from(method == "has"),
            },
            &[],
            &[Type::I32],
        );
        let helpers = self
            .registry
            .http_helpers
            .and_then(|helpers| helpers.fetch)
            .context("Headers requires a supported HTTP source operation")?;
        let payload = self.call_completion(helpers.headers, &[headers, name, query]);
        Ok(abi::decode_payload(
            &mut self.body,
            self.block,
            payload,
            true,
        ))
    }

    fn fetch_property(&mut self, receiver: &Expr, property: &str) -> Result<Value> {
        if property == "headers" {
            return self.expression(receiver);
        }
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
