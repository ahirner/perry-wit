#[path = "support/http_fixture.rs"]
mod http_fixture;
mod support;

use http_fixture::{HttpFixture, Reply, ServingComponent};
use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn incoming_component_serves_awaited_text_binary_and_error_paths() {
    let scratch = support::Scratch::new();
    let compiled = perry_wit::compiler::compile_typescript(r#"
        let calls = 0;
        export async function incomingHandlerHandle(request: any): Promise<any> {
            calls++;
            await new Promise(resolve => setTimeout(resolve, 1));
            if (request.url.endsWith("/reject")) { throw "intentional rejection"; }
            if (request.url.endsWith("/wrong")) { return "not a response"; }
            if (request.url.endsWith("/binary")) {
                return new Response(await request.bytes(), {status:201, headers:{"x-call":"" + calls}});
            }
            if (request.url.endsWith("/json")) {
                try { const value = await request.json(); return new Response(JSON.stringify(value)); }
                catch (error) { return new Response("caught:" + error, {status:400}); }
            }
            return new Response(request.method + ":" + await request.text(), {
                status:202, headers:{"x-call":"" + calls, "x-input":request.headers.get("x-test"), "x-url":request.url}
            });
        }
    "#, "handler.ts", &perry_wit::compiler::CompileOptions {
        world:Some("http-server".into()), ..Default::default()
    }).unwrap();
    let path = scratch.0.join("incoming.wasm");
    fs::write(&path, compiled.component.unwrap()).unwrap();
    let host = ServingComponent::new(&path, &support::get_wasmtime_path());
    let script = r#"
        import assert from 'node:assert/strict';
        const base = process.argv[1];
        const options = (body) => ({method:'POST', headers:{'x-test':'sample'}, body, signal:AbortSignal.timeout(5000)});
        const response = await fetch(base + '/text?test=1', options('hé😀'));
        assert.equal(response.status, 202);
        assert.equal(response.headers.get('x-call'), '1');
        assert.equal(response.headers.get('x-input'), 'sample');
        assert.equal(response.headers.get('x-url'), base + '/text?test=1');
        assert.equal(await response.text(), 'POST:hé😀');
        const latin1 = await fetch(base + '/text', {...options('header'), headers:{'x-test':'é'}});
        assert.equal(latin1.headers.get('x-input'), 'é');
        await latin1.text();
        const bytes = new Uint8Array(131073);
        for (let i = 0; i < bytes.length; i++) {bytes[i] = i;}
        const binary = await fetch(base + '/binary', options(bytes));
        assert.equal(binary.status, 201);
        assert.equal(binary.headers.get('x-call'), '3');
        assert.deepEqual(new Uint8Array(await binary.arrayBuffer()), bytes);
        const valid = await fetch(base + '/json', options('{"text":"ok"}'));
        assert.equal(await valid.text(), '{"text":"ok"}');
        const invalid = await fetch(base + '/json', options('{'));
        assert.equal(invalid.status, 400);
        assert.match(await invalid.text(), /^caught:SyntaxError:/);
        for (const path of ['/reject', '/wrong']) {
            const failed = await fetch(base + path, options('body'));
            assert.equal(failed.status, 500);
            await failed.text();
        }
        const tooLarge = await fetch(base + '/binary', options(new Uint8Array(1048577)));
        assert.equal(tooLarge.status, 500);
        await tooLarge.text();
        const recovered = await fetch(base + '/text', options('again'));
        assert.equal(recovered.status, 202);
        assert.equal(await recovered.text(), 'POST:again');
        for (let i = 0; i < 30; i++) {
            const response = await fetch(base + '/text', options('repeat'));
            assert.equal(await response.text(), 'POST:repeat');
        }
    "#;
    let output = Command::new("node")
        .args(["--input-type=module", "-e", script])
        .arg(format!("http://{}", host.address))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(path.with_extension("serve.log")).unwrap()
    );
}

#[test]
fn incoming_component_streams_before_upload_completion_and_recovers_after_disconnect() {
    let scratch = support::Scratch::new();
    let compiled = perry_wit::compiler::compile_typescript(
        r#"
        let tracker = {count:0};
        export async function incomingHandlerHandle(request: any): Promise<any> {
            if (request.url.endsWith("/state")) { return new Response("" + tracker.count); }
            if (request.url.endsWith("/static")) {
                const response = new Response(Uint8Array.from([0, 255, 128, 7]));
                return new Response(response.body);
            }
            tracker.count = 0;
            setTimeout(() => {tracker.count++;}, 1);
            await 0;
            return new Response(request.body, {status:201, headers:{"x-flow":"stream"}});
        }
    "#,
        "streaming.ts",
        &perry_wit::compiler::CompileOptions {
            world: Some("http-server".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let path = scratch.0.join("streaming.wasm");
    fs::write(&path, compiled.component.unwrap()).unwrap();
    let host = ServingComponent::new(&path, &support::get_wasmtime_path());
    let script = r#"
        import assert from 'node:assert/strict';
        import http from 'node:http';
        import {once} from 'node:events';
        const base = process.argv[1];
        const bytes = Buffer.alloc(2097153);
        for (let i = 0; i < bytes.length; i++) {bytes[i] = i;}
        await new Promise((resolve, reject) => {
            let uploaded = false, first = true;
            const chunks = [];
            const request = http.request(base + '/forward', {
                method:'POST', headers:{'content-length':bytes.length}, agent:false
            }, response => {
                assert.equal(response.statusCode, 201);
                assert.equal(response.headers['x-flow'], 'stream');
                response.on('error', reject);
                response.on('data', chunk => {
                    chunks.push(chunk);
                    if (first) {
                        first = false;
                        assert.equal(uploaded, false, 'first response bytes must precede upload completion');
                        (async () => {
                            for (let offset = 16384; offset < bytes.length; offset += 16384) {
                                if (!request.write(bytes.subarray(offset, offset + 16384))) {await once(request, 'drain');}
                                await new Promise(resolve => setTimeout(resolve, 1));
                            }
                            uploaded = true;
                            request.end();
                        })().catch(reject);
                    }
                    response.pause();
                    setTimeout(() => response.resume(), 2);
                });
                response.on('end', () => {
                    clearTimeout(deadline);
                    assert.equal(uploaded, true);
                    assert.deepEqual(Buffer.concat(chunks), bytes);
                    resolve();
                });
            });
            const deadline = setTimeout(() => request.destroy(new Error('stream did not make progress')), 10000);
            request.on('error', error => {clearTimeout(deadline); reject(error);});
            // Hold the rest of the upload until the server returns its first body chunk.
            request.write(bytes.subarray(0, 16384));
        });
        const delivered = await fetch(base + '/state', {signal:AbortSignal.timeout(5000)});
        assert.equal(await delivered.text(), '1', 'blocked I/O permits timer delivery');
        for (let i = 0; i < 3; i++) {
            await new Promise((resolve, reject) => {
                let canceled = false;
                const request = http.request(base + '/cancel', {
                    method:'POST', headers:{'content-length':bytes.length}, agent:false
                }, response => {
                    response.on('error', error => {if (!canceled) {reject(error);}});
                    response.once('data', () => {
                        canceled = true;
                        clearTimeout(deadline);
                        response.destroy(); request.destroy(); resolve();
                    });
                });
                const deadline = setTimeout(() => request.destroy(new Error('cancel did not make progress')), 5000);
                request.on('error', error => {clearTimeout(deadline); if (!canceled) {reject(error);}});
                request.write(bytes.subarray(0, 16384));
            });
            const state = await fetch(base + '/state', {signal:AbortSignal.timeout(5000)});
            assert.match(await state.text(), /^[01]$/, 'server recovers after cancellation');
        }
        const fixed = await fetch(base + '/static', {signal:AbortSignal.timeout(5000)});
        assert.deepEqual(new Uint8Array(await fixed.arrayBuffer()), Uint8Array.from([0, 255, 128, 7]));
    "#;
    let output = Command::new("node")
        .args(["--input-type=module", "-e", script])
        .arg(format!("http://{}", host.address))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(path.with_extension("serve.log")).unwrap()
    );
}

#[test]
fn response_construction_preserves_bytes_and_reports_unsupported_forms() {
    let source = r#"
        async function report() {
            const bytes = Uint8Array.from([7, 0, 255, 8]);
            const response = new Response(bytes.subarray(1, -1), {status:201, headers:{"X-Test":" é "}});
            bytes[1] = 99;
            console.log(response.status); console.log(response.ok); console.log(response.headers.get("x-test"));
            console.log(JSON.stringify(response)); console.log("status" in response);
            console.log(JSON.stringify(await response.bytes()));
            console.log(await new Response(null, {status:204}).text());
            console.log(await new Response("hé😀").text());
            const forwarded = new Response(new Response(Uint8Array.from([0, 255, 128])).body);
            console.log(JSON.stringify(await forwarded.bytes()));
        }
        report();
    "#;
    let reference = Command::new("node")
        .args(["--eval", source])
        .output()
        .unwrap();
    let output = support::run(source, None, None);
    assert_eq!(support::stdout(&output), support::stdout(&reference));
    let output = support::run(
        r#"
        const source = Uint8Array.from([7, 0, 255, 8]);
        const response = new Response(source.subarray(1, -1), {status:201, headers:{"X-Test":"sample"}});
        source[1] = 99;
        console.log(response.status); console.log(response.ok); console.log(response.headers.get("x-test"));
        console.log(JSON.stringify(response)); console.log("status" in response);
        console.log(JSON.stringify(response.bytes()));
        const empty = new Response(null, {status:204}); console.log(empty.text());
        const text = new Response("hé😀"); console.log(text.text());
        try { new Response("bad", {status:204}); } catch (error) { console.log(error); }
        try { new Response("bad", {status:199}); } catch (error) { console.log(error); }
        try { new Response("bad", {statusText:"reason"}); } catch (error) { console.log(error); }
        try { new Response([1, 2]); } catch (error) { console.log(error); }
        try { new Response(new Uint8Array(1048577)); } catch (error) { console.log(error); }
        try { text.arrayBuffer(); } catch (error) { console.log(error); }
        try { text.body.getReader(); } catch (error) { console.log(error); }
    "#,
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "201\ntrue\nsample\n{}\ntrue\n{\"0\":0,\"1\":255}\n\nhé😀\nTypeError: Response status cannot have a body\nRangeError: Invalid Response status\nTypeError: Unsupported Response option: statusText\nTypeError: Response body must be a string or Uint8Array\nRangeError: Response body exceeds the 1048576 byte limit\nTypeError: Unsupported buffered HTTP method: arrayBuffer\nTypeError: Body streams currently support forwarding through Response; reader methods are not implemented\n"
    );
}

#[test]
fn incoming_world_diagnoses_missing_and_incompatible_implementations() {
    for (source, diagnostic) in [
        (
            "export function unrelated() { return 'value'; }",
            "missing implementation 'incomingHandlerHandle'",
        ),
        (
            "export function incomingHandlerHandle(request: any, extra: any) { return new Response('body'); }",
            "must accept one Request",
        ),
    ] {
        let error = perry_wit::compiler::compile_typescript(
            source,
            "invalid.ts",
            &perry_wit::compiler::CompileOptions {
                world: Some("http-server".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains(diagnostic), "{error:#}");
    }
}

#[test]
fn incoming_resources_and_memory_stay_bounded_in_one_instance() {
    let scratch = support::Scratch::new();
    let compiled = perry_wit::compiler::compile_typescript(r#"
        let saved: any = null;
        export async function incomingHandlerHandle(request: any): Promise<any> {
            if (request.url.endsWith("/retain")) { saved = request; }
            if (request.url.endsWith("/saved")) { return new Response(saved.bytes(), {status:201}); }
            if (request.url.endsWith("/release")) { saved = null; }
            await new Promise(resolve => setTimeout(resolve, 1));
            if (request.url.endsWith("/reject")) { throw "intentional"; }
            if (request.url.endsWith("/pending")) { return new Promise(() => {}); }
            return new Response(await request.bytes(), {status:201, headers:request.headers});
        }
    "#, "handler.ts", &perry_wit::compiler::CompileOptions {
        world:Some("http-server".into()), ..Default::default()
    }).unwrap();
    let path = scratch.0.join("incoming.core.wasm");
    fs::write(&path, compiled.core).unwrap();
    let failed = perry_wit::compiler::compile_typescript(r#"
        throw "initializer failed";
        export function incomingHandlerHandle(request: any): any { return new Response("unreachable"); }
    "#, "failed.ts", &perry_wit::compiler::CompileOptions {
        world:Some("http-server".into()), ..Default::default()
    }).unwrap();
    let failed_path = scratch.0.join("failed.core.wasm");
    fs::write(&failed_path, failed.core).unwrap();
    let streaming = perry_wit::compiler::compile_typescript(r#"
        let saved: any = null;
        let oldRequest: any = null;
        let tracker = {count:0};
        async function bump() { await 0; tracker.count++; }
        export async function incomingHandlerHandle(request: any): Promise<any> {
            if (request.url.endsWith("/stale")) {
                try { return new Response(saved, {status:201}); }
                catch (error) { return new Response("" + error, {status:201}); }
            }
            if (request.url.endsWith("/stale-message")) {
                try { return new Response(await oldRequest.text(), {status:201}); }
                catch (error) { return new Response("" + error, {status:201}); }
            }
            if (request.url.endsWith("/retain-unread")) {
                saved = request.body;
                oldRequest = request;
                return new Response("unread", {status:201});
            }
            if (request.url.endsWith("/marker")) { return new Response("" + tracker.count, {status:201}); }
            if (request.url.endsWith("/retain-stream")) { saved = request.body; }
            tracker.count = 0;
            if (request.url.endsWith("/cancel")) {
                setInterval(() => {tracker.count++;}, 1);
            } else if (request.url.endsWith("/throw")) {
                setTimeout(() => {throw "callback failed";}, 1);
            } else {
                setTimeout(() => {
                    bump();
                    setTimeout(() => {tracker.count++;}, 1);
                }, 1);
            }
            await 0;
            return new Response(request.body, {status:201});
        }
    "#, "streaming.ts", &perry_wit::compiler::CompileOptions {
        world:Some("http-server".into()), ..Default::default()
    }).unwrap();
    let streaming_path = scratch.0.join("streaming.core.wasm");
    fs::write(&streaming_path, streaming.core).unwrap();
    let timer_free = perry_wit::compiler::compile_typescript(
        "export function incomingHandlerHandle(request: any): any { return new Response(request.body, {status:201}); }",
        "forward.ts", &perry_wit::compiler::CompileOptions {
            world:Some("http-server".into()), ..Default::default()
        }
    ).unwrap();
    let timer_free_path = scratch.0.join("timer-free.core.wasm");
    fs::write(&timer_free_path, timer_free.core).unwrap();
    let script = r#"
        const assert = require('node:assert/strict');
        const fs = require('node:fs');
        const modules = process.argv.slice(1).map(path => new WebAssembly.Module(fs.readFileSync(path)));
        const module = modules[0];
        let exports, next = 1, now = 0n, current, failRead = false, failWrite = false, streaming = false;
        const resources = new Map();
        const view = () => new DataView(exports.memory.buffer);
        function resource(type, properties = {}) {
            const id = next++; resources.set(id, {type, ...properties}); return id;
        }
        function take(id, type) {
            assert.equal(resources.get(id)?.type, type, `ownership: ${type} ${id}`);
            assert.ok(!Array.from(resources.values()).some(child => child.parent === id), `live child of ${type}`);
            const value = resources.get(id); resources.delete(id); return value;
        }
        function result(ptr, value) {
            view().setUint8(ptr, 0);
            if (value !== undefined) {view().setUint32(ptr + 4, value, true);}
        }
        function bytes(data) {
            const ptr = exports.cabi_realloc(0, 0, 1, data.length);
            new Uint8Array(exports.memory.buffer, ptr, data.length).set(data);
            return [ptr, data.length];
        }
        function optionalString(ptr, string) {
            const [data, length] = bytes(new TextEncoder().encode(string));
            view().setUint8(ptr, 1); view().setUint32(ptr + 4, data, true); view().setUint32(ptr + 8, length, true);
        }
        const imports = {};
        for (const {module: namespace, name} of modules.flatMap(module => WebAssembly.Module.imports(module))) {
            const host = (...args) => {
                if (name.startsWith('[resource-drop]')) {
                    const value = take(args[0], name.slice('[resource-drop]'.length));
                    if (value.type === 'input-stream') {current.readBytes = value.offset;}
                    return;
                }
                switch (name) {
                    case '[method]incoming-request.method': view().setUint8(args[1], 2); return;
                    case '[method]incoming-request.scheme': view().setUint8(args[1], 1); view().setUint8(args[1] + 4, 0); return;
                    case '[method]incoming-request.authority': optionalString(args[1], 'fixture.test'); return;
                    case '[method]incoming-request.path-with-query': optionalString(args[1], current.path); return;
                    case '[method]incoming-request.headers': return resource('fields', {parent:args[0]});
                    case '[method]fields.entries': {
                        const list = exports.cabi_realloc(0, 0, 4, 16);
                        const [name, nameLen] = bytes(new TextEncoder().encode('x-test'));
                        const [value, valueLen] = bytes(new TextEncoder().encode('sample'));
                        view().setUint32(list, name, true); view().setUint32(list + 4, nameLen, true);
                        view().setUint32(list + 8, value, true); view().setUint32(list + 12, valueLen, true);
                        view().setUint32(args[1], list, true); view().setUint32(args[1] + 4, 1, true); return;
                    }
                    case '[method]incoming-request.consume': result(args[1], resource('incoming-body', {parent:args[0]})); return;
                    case '[method]incoming-body.stream': result(args[1], resource('input-stream', {parent:args[0], offset:0, ready:false})); return;
                    case '[method]input-stream.subscribe':
                    case '[method]output-stream.subscribe': return resource('pollable', {parent:args[0]});
                    case '[method]input-stream.read':
                    case '[method]input-stream.blocking-read': {
                        const stream = resources.get(args[0]);
                        if (streaming && name.endsWith('.blocking-read')) {throw new Error('streaming used a blocking read');}
                        if (streaming && !stream.ready) {
                            result(args[2], 0); view().setUint32(args[2] + 8, 0, true); return;
                        }
                        stream.ready = false;
                        if (failRead && stream.offset > 0) {
                            view().setUint8(args[2], 1); view().setUint8(args[2] + 4, 0);
                            view().setUint32(args[2] + 8, resource('error'), true); return;
                        }
                        if (stream.offset === current.input.length) {
                            view().setUint8(args[2], 1); view().setUint8(args[2] + 4, 1); return;
                        }
                        if (streaming) {
                            assert.ok(current.responded, 'publish before consuming streamed input');
                            assert.ok(Number(args[1]) <= 8192, 'bounded input request');
                            assert.equal(current.writtenBytes, stream.offset, 'output backpressure prevents input read-ahead');
                            assert.ok(!Array.from(resources.values()).some(resource => resource.type === 'output-stream' && resource.flushing), 'flush completes before the next input read');
                        }
                        const chunk = current.input.subarray(stream.offset, stream.offset + Math.min(Number(args[1]), streaming ? 8192 : 4096));
                        const [data, length] = bytes(chunk);
                        result(args[2], data); view().setUint32(args[2] + 8, length, true);
                        stream.offset += length; return;
                    }
                    case 'now': return now;
                    case 'subscribe-instant':
                        if (streaming && current.responded && Array.from(resources.values()).some(resource => resource.type === 'pollable' && resource.parent)) {
                            current.timerDuringWait++;
                        }
                        return resource('pollable', {deadline:args[0]});
                    case '[method]pollable.block': now = resources.get(args[0]).deadline; return;
                    case 'poll': {
                        assert.ok(streaming);
                        assert.ok(++current.polls < 50000, 'no readiness busy loop');
                        const ids = Array.from({length:args[1]}, (_, index) => view().getUint32(args[0] + index * 4, true));
                        const pending = ids.map(id => resources.get(id));
                        let ready = pending.findIndex(pollable => pollable.deadline !== undefined);
                        if (ready >= 0 && current.timerPolls < 2) {
                            now = pending[ready].deadline;
                            current.timerPolls++;
                        } else {
                            ready = pending.findIndex(pollable => pollable.parent !== undefined);
                            assert.ok(ready >= 0, 'an I/O subscription must own this wait');
                            const stream = resources.get(pending[ready].parent);
                            if (stream.type === 'input-stream') {stream.ready = true;}
                            else {
                                assert.equal(stream.type, 'output-stream');
                                stream.permit = ++current.grants % 2 ? 257 : 4093;
                                stream.flushing = false;
                            }
                        }
                        const data = exports.cabi_realloc(0, 0, 4, 4);
                        view().setUint32(data, ready, true);
                        view().setUint32(args[2], data, true); view().setUint32(args[2] + 4, 1, true); return;
                    }
                    case '[static]fields.from-list': result(args[2], resource('fields')); return;
                    case '[constructor]outgoing-response': take(args[0], 'fields'); return resource('outgoing-response');
                    case '[method]outgoing-response.set-status-code': resources.get(args[0]).status = args[1]; result(args[2]); return;
                    case '[method]outgoing-response.body': result(args[1], resource('outgoing-body')); return;
                    case '[method]outgoing-body.write': result(args[1], resource('output-stream', {parent:args[0], permit:streaming ? 0 : 8192, flushing:false})); return;
                    case '[static]response-outparam.set':
                        take(args[0], 'response-outparam');
                        current.responded = true;
                        current.error = args[1] !== 0;
                        if (!current.error) {current.status = take(args[2], 'outgoing-response').status;}
                        return;
                    case '[method]output-stream.blocking-write-and-flush':
                        assert.ok(current.responded, 'publish response before blocking output');
                        if (failWrite) {
                            view().setUint8(args[3], 1); view().setUint8(args[3] + 4, 0);
                            view().setUint32(args[3] + 8, resource('error'), true); return;
                        }
                        current.chunks.push(Buffer.from(new Uint8Array(exports.memory.buffer, args[1], args[2])));
                        current.writtenBytes += args[2];
                        result(args[3]); return;
                    case '[method]output-stream.check-write': {
                        if (current.path === '/cancel' && current.chunks.length > 0) {
                            view().setUint8(args[1], 1); view().setUint8(args[1] + 4, 1); return;
                        }
                        const stream = resources.get(args[0]);
                        view().setUint8(args[1], 0); view().setBigUint64(args[1] + 8, BigInt(stream.permit), true); return;
                    }
                    case '[method]output-stream.write':
                        assert.ok(current.responded, 'publish response before output');
                        if (failWrite) {
                            view().setUint8(args[3], 1); view().setUint8(args[3] + 4, 0);
                            view().setUint32(args[3] + 8, resource('error'), true); return;
                        }
                        if (streaming) {
                            const stream = resources.get(args[0]);
                            assert.ok(!stream.flushing && args[2] > 0 && args[2] <= stream.permit && args[2] <= 8192, 'respect partial write permits');
                            stream.permit = 0;
                        }
                        current.chunks.push(Buffer.from(new Uint8Array(exports.memory.buffer, args[1], args[2])));
                        current.writtenBytes += args[2];
                        result(args[3]); return;
                    case '[method]output-stream.flush': {
                        if (current.path === '/flush-error') {
                            view().setUint8(args[1], 1); view().setUint8(args[1] + 4, 0);
                            view().setUint32(args[1] + 8, resource('error'), true); return;
                        }
                        if (streaming) {
                            const stream = resources.get(args[0]); stream.flushing = true; stream.permit = 0;
                            current.flushes++;
                        }
                        result(args[1]); return;
                    }
                    case '[static]outgoing-body.finish': take(args[0], 'outgoing-body'); current.finished = true; result(args.at(-1)); return;
                    default: throw new Error(`unexpected host call: ${namespace} ${name} ${args}`);
                }
            };
            (imports[namespace] ??= {})[name] = host;
        }
        exports = new WebAssembly.Instance(module, imports).exports;
        const payload = Uint8Array.from({length:131073}, (_, index) => index);
        function invoke(path, input = payload, expected = input) {
            current = {path, input, chunks:[], responded:false, error:false, finished:false,
                polls:0, grants:0, timerPolls:0, timerDuringWait:0, flushes:0, readBytes:0, writtenBytes:0};
            exports['wasi:http/incoming-handler@0.2.6#handle'](resource('incoming-request'), resource('response-outparam'));
            assert.equal(resources.size, 0, `resources remaining after ${path}: ${JSON.stringify(Array.from(resources.values()))}`);
            assert.ok(current.responded);
            if (!current.error && !failWrite && !(streaming && (failRead || ['/cancel', '/flush-error', '/throw'].includes(path)))) {
                assert.equal(current.status, 201); assert.ok(current.finished);
                assert.deepEqual(Buffer.concat(current.chunks), Buffer.from(expected));
            }
            return current;
        }
        assert.equal(invoke('/retain').error, false);
        assert.equal(invoke('/saved').error, false);
        assert.equal(invoke('/release').error, false);
        for (let i = 0; i < 20; i++) {invoke('/body');}
        const baseline = exports.memory.buffer.byteLength;
        for (let i = 0; i < 300; i++) {
            const path = ['/body', '/reject', '/pending'][i % 3];
            assert.equal(invoke(path).error, path !== '/body');
        }
        assert.equal(exports.memory.buffer.byteLength, baseline, 'unbounded buffered handler memory');
        assert.equal(invoke('/body', new Uint8Array(1048576)).error, false);
        assert.equal(invoke('/body', new Uint8Array(1048577)).error, true);
        failRead = true; assert.equal(invoke('/body').error, true); failRead = false;
        assert.equal(invoke('/body').error, false);
        failWrite = true; assert.equal(invoke('/body').finished, false); failWrite = false;
        assert.equal(invoke('/body').error, false);
        const failedModule = modules[1];
        exports = new WebAssembly.Instance(failedModule, imports).exports;
        assert.equal(invoke('/failed').error, true);
        const failedBaseline = exports.memory.buffer.byteLength;
        for (let i = 0; i < 100; i++) {assert.equal(invoke('/failed').error, true);}
        assert.equal(exports.memory.buffer.byteLength, failedBaseline);
        streaming = true;
        exports = new WebAssembly.Instance(modules[2], imports).exports;
        const first = invoke('/retain-stream');
        assert.ok(first.timerDuringWait > 0, 'timers execute while an I/O peer remains blocked');
        assert.ok(first.polls > 0 && first.grants > 0 && first.flushes > 1);
        invoke('/marker', new Uint8Array(), Buffer.from('2'));
        invoke('/stale', new Uint8Array(), Buffer.from('TypeError: Response body stream has already been consumed'));
        const unread = invoke('/retain-unread', payload, Buffer.from('unread'));
        assert.equal(unread.readBytes, 0, 'unused incoming body closes without buffering');
        invoke('/stale', payload, Buffer.from('TypeError: Response body stream has already been consumed'));
        invoke('/stale-message', payload, Buffer.from('TypeError: Body stream has already been consumed'));
        for (let i = 0; i < 20; i++) {invoke('/stream');}
        const streamBaseline = exports.memory.buffer.byteLength;
        const large = Uint8Array.from({length:4194305}, (_, index) => index);
        invoke('/stream', large);
        assert.equal(exports.memory.buffer.byteLength, streamBaseline, 'streamed total size must not raise the guest buffer high-water mark');
        for (let i = 0; i < 50; i++) {
            const canceled = invoke('/cancel', large);
            assert.equal(canceled.finished, false);
            assert.ok(canceled.readBytes < large.length, 'early close cancels the unread source');
            assert.equal(invoke('/flush-error').finished, false);
            assert.equal(invoke('/throw').finished, false);
            failRead = true; assert.equal(invoke('/stream').finished, false); failRead = false;
            failWrite = true; assert.equal(invoke('/stream').finished, false); failWrite = false;
            invoke('/stream');
        }
        assert.equal(exports.memory.buffer.byteLength, streamBaseline, 'stream completion/failure/cancellation cycles release guest values');
        for (const {module: namespace} of WebAssembly.Module.imports(modules[3])) {
            assert.ok(!/wasi:(clocks|random|filesystem)|outgoing-handler/.test(namespace), `unused capability: ${namespace}`);
        }
        exports = new WebAssembly.Instance(modules[3], imports).exports;
        invoke('/stream', large);
        invoke('/stream', new Uint8Array());
        console.log(`Streaming: <=8192 buffered body bytes; ${streamBaseline} guest memory bytes after warm-up, 4 MiB transfer, and 50 failure/cancellation/recovery cycles`);
    "#;
    let output = Command::new("node")
        .args(["-e", script])
        .arg(path)
        .arg(failed_path)
        .arg(streaming_path)
        .arg(timer_free_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    println!("{}", String::from_utf8_lossy(&output.stdout));
}

#[test]
fn binary_bodies_preserve_subviews_empty_values_and_large_transfers() {
    let fixture = HttpFixture::new(|request| Reply::Bytes(200, request.body.clone()));
    let output = support::run(
        &format!(
            r#"
        const source = Uint8Array.from([10, 0, 255, 128, 20]);
        const view = source.subarray(1, -1);
        const options = {{ method: "POST", body: view }};
        const response = await fetch("http://{0}/view", {{ ...options }});
        const received = await response.bytes();
        console.log(JSON.stringify(received));
        console.log(received.length);
        console.log(response.status);
        received[0] = 7;
        console.log(source[1]);
        const again = await response.bytes();
        console.log(again !== received);
        console.log(again[0]);
        const empty = await fetch("http://{0}/empty", {{method:"POST", body:new Uint8Array(0)}});
        console.log((await empty.bytes()).length);
        const large = new Uint8Array(131073);
        for (let i = 0; i < large.length; i++) {{ large[i] = i; }}
        const big = await fetch("http://{0}/large", {{method:"PUT", body:large}});
        const bytes = await big.bytes();
        console.log(bytes.length);
        let equal = true;
        for (let i = 0; i < bytes.length; i++) {{ if (bytes[i] !== large[i]) {{ equal = false; }} }}
        console.log(equal);
    "#,
            fixture.address
        ),
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "{\"0\":0,\"1\":255,\"2\":128}\n3\n200\n0\ntrue\n0\n0\n131073\ntrue\n"
    );
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].body, [0, 255, 128]);
    assert!(requests[1].body.is_empty());
    assert_eq!(
        requests[2].body,
        (0..131073).map(|index| index as u8).collect::<Vec<_>>()
    );
}

#[test]
fn text_decodes_utf8_with_replacement_without_changing_cached_bytes() {
    let fixture = HttpFixture::new(|request| {
        Reply::Bytes(
            200,
            if request.target == "/json" {
                b"\xef\xbb\xbf{\"value\":\"\xff\"}".to_vec()
            } else {
                vec![239, 187, 191, 65, 255, 66]
            },
        )
    });
    let output = support::run(
        &format!(
            r#"
        const response = await fetch("http://{}/text");
        console.log(await response.text());
        console.log(JSON.stringify(await response.bytes()));
    "#,
            fixture.address
        ),
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "A�B\n{\"0\":239,\"1\":187,\"2\":191,\"3\":65,\"4\":255,\"5\":66}\n"
    );
    let output = support::run(
        &format!(
            r#"
        const response = await fetch("http://{}/json");
        console.log(JSON.stringify(await response.json()));
        console.log(await response.text());
    "#,
            fixture.address
        ),
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "{\"value\":\"�\"}\n{\"value\":\"�\"}\n"
    );
    for method in ["arrayBuffer", "blob", "formData", "clone"] {
        let output = support::run(
            &format!(
                r#"
            const response = await fetch("http://{}/unsupported");
            response.{method}(); console.log("unreachable");
        "#,
                fixture.address
            ),
            None,
            None,
        );
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("Unsupported Response method: {method}")),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn repeated_http_calls_retain_live_responses_and_release_dead_resources() {
    let scratch = support::Scratch::new();
    let wit = format!(
        r#"{}
        world test {{
            include runtime-adapter;
            export run-task: func(mode: string) -> string;
        }}
    "#,
        include_str!("../wit/world.wit")
    );
    let compiled = scratch.compile_artifacts(
        r#"
        let saved: any = null;
        export function runTask(mode: string): string {
            if (mode === "saved") { return JSON.stringify(saved.response.bytes()); }
            if (mode === "release") { saved = null; return "released"; }
            const response = fetch("https://fixture.test/binary", {
                method: "POST", body: Uint8Array.from([0, 255, 128])
            });
            if (mode === "pending") { return "pending"; }
            if (mode === "retain") { saved = { response }; return "retained:" + response.status; }
            if (mode === "headers") { return "status:" + response.status; }
            return JSON.stringify(response.bytes());
        }
    "#,
        Some(&wit),
    );
    let path = scratch.0.join("http-lifetimes.wasm");
    fs::write(&path, compiled.core).unwrap();
    let output = Command::new("node").args(["--eval", r#"
        const assert = require('node:assert/strict');
        const module = new WebAssembly.Module(require('node:fs').readFileSync(process.argv[1]));
        let exports;
        let next = 1;
        let failBody = false;
        let diagnostic = '';
        const resources = new Map();
        const payload = Uint8Array.from({length: 4096}, (_, index) => index % 256);
        const expected = JSON.stringify(payload);
        const view = () => new DataView(exports.memory.buffer);
        function resource(type) { const id = next++; resources.set(id, {type, offset:0}); return id; }
        function release(id, type) {
            assert.equal(resources.get(id)?.type, type, `resource ownership: ${id} ${type}`);
            resources.delete(id);
        }
        function result(ptr, id) {
            view().setUint8(ptr, 0);
            if (id !== undefined) { view().setUint32(ptr + 4, id, true); }
        }
        const imports = {};
        for (const {module: namespace, name} of WebAssembly.Module.imports(module)) {
            const host = (...args) => {
                if (name.startsWith('[resource-drop]')) {
                    release(args[0], name.slice('[resource-drop]'.length)); return;
                }
                switch (name) {
                    case '[static]fields.from-list': result(args[2], resource('fields')); return;
                    case '[constructor]outgoing-request':
                        release(args[0], 'fields'); return resource('outgoing-request');
                    case '[method]outgoing-request.set-method':
                    case '[method]outgoing-request.set-scheme':
                    case '[method]outgoing-request.set-authority':
                    case '[method]outgoing-request.set-path-with-query': result(args.at(-1)); return;
                    case '[method]outgoing-request.body': result(args[1], resource('outgoing-body')); return;
                    case '[method]outgoing-body.write': result(args[1], resource('output-stream')); return;
                    case 'handle':
                        release(args[0], 'outgoing-request'); result(args.at(-1));
                        view().setUint32(args.at(-1) + 8, resource('future-incoming-response'), true); return;
                    case '[method]output-stream.blocking-write-and-flush':
                        if (resources.get(args[0]).stderr) {
                            diagnostic += new TextDecoder().decode(new Uint8Array(exports.memory.buffer, args[1], args[2]));
                        } else {
                            assert.deepEqual(Array.from(new Uint8Array(exports.memory.buffer, args[1], args[2])), [0,255,128]);
                        }
                        result(args[3]); return;
                    case 'get-stderr': {
                        const id = resource('output-stream'); resources.get(id).stderr = true; return id;
                    }
                    case 'exit': throw new Error('guest exit');
                    case '[static]outgoing-body.finish': release(args[0], 'outgoing-body'); result(args.at(-1)); return;
                    case '[method]future-incoming-response.subscribe': return resource('pollable');
                    case '[method]pollable.block': return;
                    case '[method]future-incoming-response.get':
                        view().setUint8(args[1], 1);
                        view().setUint8(args[1] + 8, 0);
                        view().setUint8(args[1] + 16, 0);
                        view().setUint32(args[1] + 24, resource('incoming-response'), true); return;
                    case '[method]incoming-response.status': return 200;
                    case '[method]incoming-response.headers': return resource('fields');
                    case '[method]fields.entries':
                        view().setUint32(args[1], 0, true); view().setUint32(args[1] + 4, 0, true); return;
                    case '[method]incoming-response.consume': result(args[1], resource('incoming-body')); return;
                    case '[method]incoming-body.stream': result(args[1], resource('input-stream')); return;
                    case '[method]input-stream.blocking-read': {
                        const stream = resources.get(args[0]);
                        if (failBody && stream.offset > 0) {
                            view().setUint8(args[2], 1); view().setUint8(args[2] + 4, 0);
                            view().setUint32(args[2] + 8, resource('error'), true); return;
                        }
                        if (stream.offset === payload.length) {
                            view().setUint8(args[2], 1); view().setUint8(args[2] + 4, 1); return;
                        }
                        const chunk = payload.subarray(stream.offset, stream.offset + Math.min(1024, Number(args[1])));
                        const ptr = exports.cabi_realloc(0, 0, 1, chunk.length);
                        new Uint8Array(exports.memory.buffer, ptr, chunk.length).set(chunk);
                        result(args[2], ptr); view().setUint32(args[2] + 8, chunk.length, true);
                        stream.offset += chunk.length; return;
                    }
                    default: throw new Error(`unexpected host import: ${namespace} ${name} ${args}`);
                }
            };
            (imports[namespace] ??= {})[name] = host;
        }
        exports = new WebAssembly.Instance(module, imports).exports;
        function invoke(mode, cleanup = true) {
            const bytes = new TextEncoder().encode(mode);
            const ptr = exports.cabi_realloc(0, 0, 1, bytes.length);
            new Uint8Array(exports.memory.buffer, ptr, bytes.length).set(bytes);
            let ret;
            try { ret = exports['run-task'](ptr, bytes.length); }
            finally { exports.cabi_realloc(ptr, bytes.length, 1, 0); }
            const words = new Uint32Array(exports.memory.buffer, ret, 2);
            const text = new TextDecoder().decode(new Uint8Array(exports.memory.buffer, words[0], words[1]));
            if (cleanup) { exports['cabi_post_run-task'](ret); }
            return text;
        }
        assert.equal(invoke('retain'), 'retained:200');
        assert.ok(resources.size > 0);
        assert.equal(invoke('binary'), expected);
        assert.equal(invoke('saved'), expected);
        assert.equal(invoke('release'), 'released');
        for (let i = 0; i < 30; i++) { assert.equal(invoke('binary'), expected); }
        const baseline = exports.memory.buffer.byteLength;
        for (let i = 0; i < 600; i++) {
            const mode = ['binary', 'pending', 'headers'][i % 3];
            assert.equal(invoke(mode, i % 7 !== 0), mode === 'binary' ? expected : mode === 'pending' ? 'pending' : 'status:200');
            assert.ok(resources.size <= 1, `unreleased HTTP resources: ${resources.size}`);
        }
        assert.equal(invoke('release'), 'released');
        assert.equal(resources.size, 0);
        assert.equal(exports.memory.buffer.byteLength, baseline, 'HTTP cycles must stop growing memory after warm-up');
        failBody = true;
        assert.throws(() => invoke('binary'), /guest exit/);
        assert.match(diagnostic, /Stream error reading response/);
        assert.deepEqual(Array.from(resources.values()).map(resource => resource.type), ['incoming-response']);
        failBody = false;
        assert.equal(invoke('release'), 'released');
        assert.equal(resources.size, 0, 'the next invocation releases the failed response');
        assert.equal(invoke('binary'), expected);
    "#]).arg(&path).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn response_headers_and_methods_survive_body_consumption() {
    let fixture = HttpFixture::new(|request| {
        Reply::WithHeaders(
            202,
            vec![
                ("X-Result".into(), request.method.clone()),
                ("X-List".into(), "one".into()),
                ("X-List".into(), "two".into()),
                ("X-Empty".into(), "".into()),
            ],
            "accepted".into(),
        )
    });
    let output = support::run(
        &format!(
            r#"
        const methods = ["put", "PATCH", "patch", "PaTcH", "DELETE", "HEAD", "OPTIONS", "custom"];
        for (let i = 0; i < methods.length; i++) {{
            const response = await fetch("http://{0}/metadata", {{method: methods[i]}});
            console.log(response.status);
            console.log(response.statusText === "");
            const headers = response.headers;
            console.log(headers === response.headers);
            console.log(headers.get("X-RESULT"));
            console.log(headers.has("x-result"));
            console.log(headers.get("missing") === null);
            console.log(headers.get("x-list"));
            console.log(headers.has("missing"));
            console.log(headers.has("x-empty"));
            console.log(headers.get("x-empty") === "");
            console.log(response.url === "http://{0}/metadata");
            console.log(await response.text());
            console.log(response.headers.get("x-result"));
        }}
    "#,
            fixture.address
        ),
        None,
        None,
    );
    let methods = [
        "PUT", "PATCH", "patch", "PaTcH", "DELETE", "HEAD", "OPTIONS", "custom",
    ];
    let expected: String = methods
        .iter()
        .map(|method| {
            format!(
                "202\ntrue\ntrue\n{method}\ntrue\ntrue\none, two\nfalse\ntrue\ntrue\ntrue\n{}\n{method}\n",
                if *method == "HEAD" { "" } else { "accepted" }
            )
        })
        .collect();
    assert_eq!(support::stdout(&output), expected);
    assert_eq!(
        fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        methods
    );
}

#[test]
fn fetch_in_class_method_selects_http_dispatch() {
    let fixture = HttpFixture::new(|_| Reply::Body(200, "class response".into()));
    let output = support::run(
        &format!(
            r#"
            class Client {{ static request() {{ return fetch("http://{}"); }} }}
            const response = await Client.request();
            console.log(await response.text());
        "#,
            fixture.address
        ),
        None,
        None,
    );
    assert_eq!(support::stdout(&output), "class response\n");
}

#[test]
fn http_error_statuses_resolve_with_status_and_readable_body() {
    let fixture = HttpFixture::new(|request| {
        Reply::WithHeaders(
            request.target[1..].parse().unwrap(),
            vec![("X-Status".into(), request.target[1..].into())],
            "error body".into(),
        )
    });
    for status in [200, 404, 500] {
        let output = support::run(
            &format!(
                r#"
            const response = await fetch("http://{}/{status}");
            console.log(response.status);
            console.log(response.ok);
            console.log(response.headers.get("x-status"));
            console.log(await response.text());
            console.log(response.headers.get("X-STATUS"));
        "#,
                fixture.address
            ),
            None,
            None,
        );
        assert_eq!(
            support::stdout(&output),
            format!(
                "{status}\n{}\n{status}\nerror body\n{status}\n",
                status == 200
            )
        );
    }
}

#[test]
fn response_header_errors_report_diagnostics_instead_of_dummy_values() {
    let fixture = HttpFixture::new(|_| Reply::Body(200, "ok".into()));
    for (operation, diagnostic) in [
        (r#"response.headers.get("")"#, "Invalid HTTP header name"),
        (
            r#"response.headers.has("bad name")"#,
            "Invalid HTTP header name",
        ),
        (
            r#"response.headers.set("x-name", "value")"#,
            "Unsupported Headers method: set",
        ),
        (
            r#"response.headers.entries()"#,
            "Unsupported Headers method: entries",
        ),
    ] {
        let output = support::run(
            &format!(
                r#"
            const response = await fetch("http://{}/");
            {operation};
            console.log("continued");
        "#,
                fixture.address
            ),
            None,
            None,
        );
        assert!(!output.status.success(), "{operation}");
        assert!(output.stdout.is_empty(), "{operation}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(diagnostic),
            "{operation}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(fixture.requests.lock().unwrap().len(), 4);
}

#[test]
fn response_json_preserves_primitive_and_composite_values() {
    let fixture = HttpFixture::new(|request| {
        let body = match request.target.as_str() {
            "/number" => "42",
            "/boolean" => "true",
            "/string" => "\"hello\"",
            "/null" => "null",
            "/array" => "[7]",
            "/object" => "{\"value\":8}",
            _ => panic!("unexpected request: {request:?}"),
        };
        Reply::Body(200, body.into())
    });
    let output = support::run(
        &format!(
            r#"
            const numberResponse = await fetch("http://{0}/number");
            const numberValue = await numberResponse.json();
            console.log(numberValue * 2);
            console.log(numberValue + 1);
            const booleanResponse = await fetch("http://{0}/boolean");
            console.log((await booleanResponse.json()) === true);
            const stringResponse = await fetch("http://{0}/string");
            console.log((await stringResponse.json()) + "!");
            const nullResponse = await fetch("http://{0}/null");
            console.log((await nullResponse.json()) === null);
            const arrayResponse = await fetch("http://{0}/array");
            const arrayValue = await arrayResponse.json();
            console.log(arrayValue[0] * 2);
            const objectResponse = await fetch("http://{0}/object");
            const objectValue = await objectResponse.json();
            console.log(objectValue.value * 2);
        "#,
            fixture.address
        ),
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "84\n43\ntrue\nhello!\ntrue\n14\n16\n"
    );
}

#[test]
fn fetch_forwards_static_and_dynamic_options_and_complete_bodies() {
    let fixture = HttpFixture::new(|_| Reply::Body(200, "accepted".into()));
    let body = "é".repeat(5000);
    let output = support::run(
        &format!(
            r#"
            const first = await fetch("http://{0}/submit?key=value#ignored", {{
                method: "POST", headers: {{ "x-static": "yes" }}, body: "{body}"
            }});
            console.log(await first.text());
            const headers = {{ "x-dynamic": "yes", "content-type": "text/plain" }};
            const options = {{ method: "post", headers: headers, body: "second" }};
            const second = await fetch("http://{0}", options);
            console.log(await second.text());
            const third = await fetch("http://{0}?get=yes", {{ headers: [["x-pair", "yes"]] }});
            console.log(await third.text());
        "#,
            fixture.address
        ),
        None,
        None,
    );
    assert_eq!(support::stdout(&output), "accepted\naccepted\naccepted\n");
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].target, "/submit?key=value");
    assert_eq!(requests[0].body, body.as_bytes());
    assert!(
        requests[0]
            .headers
            .contains(&("x-static".into(), "yes".into()))
    );
    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[1].body, b"second");
    assert!(
        requests[1]
            .headers
            .contains(&("x-dynamic".into(), "yes".into()))
    );
    assert!(
        requests[1]
            .headers
            .contains(&("content-type".into(), "text/plain".into()))
    );
    assert_eq!(requests[2].method, "GET");
    assert_eq!(requests[2].target, "/?get=yes");
    assert!(
        requests[2]
            .headers
            .contains(&("x-pair".into(), "yes".into()))
    );
}

#[test]
fn unsupported_fetch_options_fail_before_sending_a_request() {
    let fixture = HttpFixture::new(|_| Reply::Body(200, "unexpected".into()));
    for (options, message) in [
        (r#"{ cache: "reload" }"#, "Unsupported fetch option: cache"),
        (
            r#"{ redirect: "follow" }"#,
            "Unsupported fetch option: redirect",
        ),
        ("{ signal: null }", "Unsupported fetch option: signal"),
        (r#"{ method: "CONNECT" }"#, "Forbidden fetch method"),
        (r#"{ method: "bad method" }"#, "Invalid fetch method"),
        (r#"{ method: "POST", body: 42 }"#, "body must be a string"),
        (
            r#"{ method: "POST", body: [0,255] }"#,
            "body must be a string or Uint8Array",
        ),
        (
            r#"{ method: "POST", body: { "0": 255 } }"#,
            "body must be a string or Uint8Array",
        ),
        (
            "{ headers: { wrong: 42 } }",
            "header values must be strings",
        ),
        (
            r#"{ body: "invalid GET body" }"#,
            "GET and HEAD requests cannot have a body",
        ),
        (
            r#"{ method: "HEAD", body: "" }"#,
            "GET and HEAD requests cannot have a body",
        ),
        (
            "{ body: new Uint8Array(0) }",
            "GET and HEAD requests cannot have a body",
        ),
        (
            "{ method: 'HEAD', body: Uint8Array.from([1]) }",
            "GET and HEAD requests cannot have a body",
        ),
    ] {
        let output = support::run(
            &format!(
                r#"await fetch("http://{}/", {options}); console.log("continued");"#,
                fixture.address
            ),
            None,
            None,
        );
        assert!(!output.status.success(), "options should fail: {options}");
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(fixture.requests.lock().unwrap().is_empty());
}

fn run_bounded(wasm: &Path) -> Output {
    let mut child = Command::new(support::get_wasmtime_path())
        .args([
            "run",
            "-C",
            "cache=n",
            "-S",
            "http=y",
            "-S",
            "inherit-network=y",
        ])
        .arg(wasm)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "request remained blocked: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn promise_all_observes_later_failure_while_earlier_request_stalls() {
    let fixture = HttpFixture::new(|request| match request.target.as_str() {
        "/stall" => Reply::Stall,
        "/body" => Reply::StallBody,
        "/fail" => Reply::Disconnect,
        _ => panic!("unexpected request: {request:?}"),
    });
    for path in ["stall", "body"] {
        let scratch = support::Scratch::new();
        let wasm = scratch.compile(
            &format!(
                r#"
                await Promise.all([fetch("http://{0}/{path}"), fetch("http://{0}/fail")]);
                console.log("continued");
            "#,
                fixture.address
            ),
            None,
        );
        let output = run_bounded(&wasm);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("HTTP request") && error.contains("/fail"),
            "{error}"
        );
    }
}

#[test]
fn promise_all_retains_input_order_and_fetch_resolves_before_body_completion() {
    let fixture = HttpFixture::new(|request| match request.target.as_str() {
        "/first" => {
            thread::sleep(Duration::from_millis(100));
            Reply::Body(200, "first".into())
        }
        "/second" => Reply::Body(200, "second".into()),
        "/body" => Reply::StallBody,
        _ => panic!("unexpected request: {request:?}"),
    });
    let scratch = support::Scratch::new();
    let wasm = scratch.compile(
        &format!(
            r#"
            const first = fetch("http://{0}/first");
            const second = fetch("http://{0}/second");
            const responses = await Promise.all([first, second, first]);
            console.log(await responses[0].text());
            console.log(await responses[1].text());
            console.log(await responses[2].text());
            const empty = await Promise.all([]);
            console.log(empty.length);
            const response = await fetch("http://{0}/body");
            console.log(response.status);
        "#,
            fixture.address
        ),
        None,
    );
    assert_eq!(
        support::stdout(&run_bounded(&wasm)),
        "first\nsecond\nfirst\n0\n200\n"
    );
}
