//! Runtime bindings of HIR import specifiers.
use perry_mir::Intrinsic;
use perry_wit_diagnostics::{Result, bail_unsupported};

#[derive(Clone, Copy)]
pub(crate) enum Imported<'s> {
    Timer(&'s str),
    TimeoutPromise,
    ReadFile,
    WriteFile,
    Path(Intrinsic, &'static str),
    Writable,
    Service(Intrinsic),
    Foreign(&'s str, &'s crate::wit::ImportSignature<'s>),
    Other,
}

#[derive(Clone, Copy)]
pub(super) enum Module {
    Timers,
    PromiseTimers,
    Fs,
    Stream,
    Http,
    Other,
}

impl Module {
    pub(super) fn classify(source: &str) -> Result<Self> {
        Ok(match source {
            _ if crate::hir_compat::is_timer_module(source) => Self::Timers,
            "timers/promises" | "node:timers/promises" => Self::PromiseTimers,
            "fs/promises" | "node:fs/promises" => Self::Fs,
            "stream" | "node:stream" => Self::Stream,
            "perry:http-server" => Self::Http,
            "fs" | "node:fs" => bail_unsupported!(
                "Unsupported filesystem import {source}: synchronous and callback APIs are not supported; use node:fs/promises"
            ),
            _ if source.starts_with("node:") || matches!(source, "process" | "crypto") => {
                bail_unsupported!(
                    "Unsupported capability import {source}: supported Node imports are callback timers from node:timers, setTimeout from node:timers/promises, asynchronous filesystem operations from node:fs/promises and Writable from node:stream"
                )
            }
            _ => Self::Other,
        })
    }

    pub(super) fn resolve<'s>(self, source: &str, name: Option<&'s str>) -> Result<Imported<'s>> {
        use Imported::*;
        let name = match (self, name) {
            (Self::Other, _) | (Self::Timers, None) => return Ok(Other),
            (Self::Timers, Some(name)) => return Ok(Timer(name)),
            (_, Some(name)) => name,
            _ => bail_unsupported!("only named imports from {source} are supported yet"),
        };
        Ok(match (self, name) {
            (Self::PromiseTimers, "setTimeout") => TimeoutPromise,
            (Self::Fs, "readFile") => ReadFile,
            (Self::Fs, "writeFile") => WriteFile,
            (Self::Fs, "stat") => Path(Intrinsic::FsStat, "Stat"),
            (Self::Fs, "readdir") => Path(Intrinsic::FsReaddir, "Readdir"),
            (Self::Fs, "mkdir") => Path(Intrinsic::FsMkdir, "Mkdir"),
            (Self::Fs, "unlink") => Path(Intrinsic::FsUnlink, "Unlink"),
            (Self::Fs, "rmdir") => Path(Intrinsic::FsRmdir, "Rmdir"),
            (Self::Stream, "Writable") => Writable,
            (Self::Http, "request") => Service(Intrinsic::ServeRequest),
            (Self::Http, "body") => Service(Intrinsic::ServeBody),
            (Self::Http, "method") => Service(Intrinsic::ServeMethod),
            (Self::Http, "methodOther") => Service(Intrinsic::ServeMethodOther),
            (Self::Http, "hasScheme") => Service(Intrinsic::ServeHasScheme),
            (Self::Http, "scheme") => Service(Intrinsic::ServeScheme),
            (Self::Http, "schemeOther") => Service(Intrinsic::ServeSchemeOther),
            (Self::Http, "authority") => Service(Intrinsic::ServeAuthority),
            (Self::Http, "path") => Service(Intrinsic::ServePath),
            (Self::Http, "headerCount") => Service(Intrinsic::ServeHeaderCount),
            (Self::Http, "headerName") => Service(Intrinsic::ServeHeaderName),
            (Self::Http, "headerValue") => Service(Intrinsic::ServeHeaderValue),
            (Self::Http, "addHeader") => Service(Intrinsic::ServeAddHeader),
            (Self::Http, "respond") => Service(Intrinsic::ServeRespond),
            _ => bail_unsupported!("{name} from {source} is not supported yet"),
        })
    }
}
