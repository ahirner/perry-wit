//! Runtime and helper-library functions that compiled code imports, each declared once
//! with its Wasm signature. Calls name a [Runtime] variant; the import is emitted once per
//! module for every function used.
use pliron::{
    builtin::types::{FP64Type, IntegerType, Signedness},
    context::Context,
    identifier::Identifier,
    r#type::TypeHandle,
};

/// Wasm value types of runtime signatures.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum W {
    I32,
    I64,
    F64,
}

impl W {
    fn mir(self, ctx: &mut Context) -> TypeHandle {
        match self {
            W::I32 => IntegerType::get(ctx, 32, Signedness::Signless).into(),
            W::I64 => IntegerType::get(ctx, 64, Signedness::Signless).into(),
            W::F64 => FP64Type::get(ctx).into(),
        }
    }
}

macro_rules! runtime_functions {
    ($($variant:ident = $name:literal ($($param:ident),*) $(-> $result:ident)?;)*) => {
        /// A function of the runtime or of a helper library.
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
        pub(crate) enum Runtime {
            $($variant,)*
        }

        impl Runtime {
            /// The exported name in the runtime or helper library.
            pub(crate) fn name(self) -> &'static str {
                match self {
                    $(Runtime::$variant => $name,)*
                }
            }

            pub(crate) fn params(self) -> &'static [W] {
                match self {
                    $(Runtime::$variant => &[$(W::$param),*],)*
                }
            }

            /// The single result, if the function returns one.
            pub(crate) fn result(self) -> Option<W> {
                match self {
                    $(Runtime::$variant => runtime_functions!(@result $($result)?),)*
                }
            }
        }
    };
    (@result) => { None };
    (@result $result:ident) => { Some(W::$result) };
}

runtime_functions! {
    // Core runtime services, in their established import order.
    Alloc = "perry_alloc" (I32) -> I32;
    ArrayNew = "perry_array_new" (I32) -> I32;
    ArrayGrow = "perry_array_grow" (I32);
    F64Rem = "perry_f64_rem" (F64, F64) -> F64;
    TaskEnter = "perry_task_enter" () -> I32;
    TaskResume = "perry_task_resume" (I32);
    TaskSuspend = "perry_task_suspend" () -> I32;
    TaskFree = "perry_task_free" ();
    TaskJoin = "perry_task_join" (I32);
    TaskSubtaskEvent = "perry_task_subtask_event" (I32, I32);
    TaskCloseSet = "perry_task_close_set" ();
    TaskResult = "perry_task_result" () -> I32;
    TaskSetResult = "perry_task_set_result" (I32);
    PromiseEvent = "perry_promise_event" (I32, I32) -> I32;
    PromiseNext = "perry_promise_next" (I32) -> I32;
    CombinatorStep = "perry_combinator_step" (I32);
    SubtaskRegister = "perry_subtask_register" (I32, I32, I64);
    PromiseNew = "perry_promise_new" () -> I32;
    PromiseResolve = "perry_promise_resolve" (I32, I64);
    PromiseReject = "perry_promise_reject" (I32, I32);
    PromiseWait = "perry_promise_wait" (I32, I32, I32);
    PromiseRethrow = "perry_promise_rethrow" (I32) -> I32;
    ExceptionNew = "perry_exception_new" (I32, I32, I64) -> I32;
    ExceptionRaise = "perry_exception_raise" (I32) -> I32;
    ExceptionCatch = "perry_exception_catch" (I32) -> I32;
    ExceptionMessage = "perry_exception_message" (I32) -> I32;
    Exit = "perry_exit" (F64) -> I32;
    CabiRealloc = "perry_cabi_realloc" (I32, I32, I32, I32) -> I32;
    StringAdopt = "perry_string_adopt" (I32, I32) -> I32;
    StringConcat = "perry_string_concat" (I32, I32) -> I32;
    StringCompare = "perry_string_compare" (I32, I32) -> I32;
    Print = "perry_print" (I32, I32, I32);
    IoPump = "perry_io_pump" ();
    IoEvent = "perry_io_event" (I32, I32, I32) -> I32;
    IoClose = "perry_io_close" () -> I32;
    ExitCodeSet = "perry_exit_code_set" (F64);
    CommandOutcome = "perry_command_outcome" (I32) -> I32;
    RejectionText = "perry_rejection_text" (I32) -> I32;
    AbortNew = "perry_abort_new" () -> I32;
    Abort = "perry_abort" (I32, I32, I32);
    AbortAborted = "perry_abort_aborted" (I32) -> I32;
    AbortReject = "perry_abort_reject" (I32, I32);
    AbortWatch = "perry_abort_watch" (I32, I32, I32);
    EnvGet = "perry_env_get" (I32) -> I32;
    EnvKeys = "perry_env_keys" () -> I32;
    PerformanceNow = "perry_performance_now" () -> F64;
    DateNow = "perry_date_now" () -> F64;
    NumberFormat = "number_format" (F64, I32) -> I32;
    // Further runtime and helper-library functions, by name.
    FsClose = "fs_close" () -> I32;
    FsEvent = "fs_event" (I32, I32, I32) -> I32;
    FsMkdir = "fs_mkdir" (I32) -> I32;
    FsReadBytes = "fs_read_bytes" (I32) -> I32;
    FsReadText = "fs_read_text" (I32) -> I32;
    FsReaddir = "fs_readdir" (I32) -> I32;
    FsRmdir = "fs_rmdir" (I32) -> I32;
    FsStat = "fs_stat" (I32) -> I32;
    FsStatsIsDirectory = "fs_stats_is_directory" (I32) -> I32;
    FsStatsIsFile = "fs_stats_is_file" (I32) -> I32;
    FsStatsSize = "fs_stats_size" (I32) -> F64;
    FsUnlink = "fs_unlink" (I32) -> I32;
    FsWriteBytes = "fs_write_bytes" (I32, I32) -> I32;
    FsWriteText = "fs_write_text" (I32, I32) -> I32;
    JsonMeasure = "json_measure" (I32, I32) -> I64;
    JsonPopulate = "json_populate_measured" (I32, I32, I32, I32) -> I64;
    ArrayGrowCollectible = "perry_array_grow_collectible" (I32, I32);
    ArrayNewCollectible = "perry_array_new_collectible" (I32, I32) -> I32;
    CabiRelease = "perry_cabi_release" (I32);
    ForeignCancelled = "perry_foreign_cancelled" () -> I32;
    ForeignRegister = "perry_foreign_register" (I32, I32, I64);
    FrameFree = "perry_frame_free" (I32);
    GcAlloc = "perry_gc_alloc" (I32, I32) -> I32;
    GcBoundary = "perry_gc_boundary" ();
    GcCheck = "perry_gc_check" ();
    GcDue = "perry_gc_due" () -> I32;
    GcCounter = "perry_gc_counter" (I32) -> I64;
    GcDebug = "perry_gc_debug" (I32);
    GcGlobal = "perry_gc_global" (I32);
    GcPin = "perry_gc_pin" (I32) -> I32;
    GcTaskEnd = "perry_gc_task_end" ();
    GcTaskEnter = "perry_gc_task_enter" ();
    GcTaskLeave = "perry_gc_task_leave" ();
    GcUnpin = "perry_gc_unpin" (I32);
    IoCancel = "perry_io_cancel" ();
    PromiseAll = "perry_promise_all" (I32) -> I32;
    PromiseAllSettled = "perry_promise_all_settled" (I32) -> I32;
    PromiseCombineCollectible = "perry_promise_combine_collectible" (I32, I32, I32) -> I32;
    PromiseNewCollectible = "perry_promise_new_collectible" (I32) -> I32;
    PromiseRace = "perry_promise_race" (I32) -> I32;
    ResourceDrop = "perry_resource_drop" (I32);
    ResourceDropNext = "perry_resource_drop_next" () -> I64;
    ResourceHandle = "perry_resource_handle" (I32) -> I32;
    ResourceIsRep = "perry_resource_is_rep" (I32) -> I32;
    ResourceLower = "perry_resource_lower" (I32, I32, I32) -> I32;
    ResourceNew = "perry_resource_new" (I32, I32, I32) -> I32;
    ResourceScopeCommit = "perry_resource_scope_commit" (I32);
    ResourceScopeFinish = "perry_resource_scope_finish" (I32);
    ResourceScopeNew = "perry_resource_scope_new" () -> I32;
    ResourceTaskReturn = "perry_resource_task_return" ();
    RootsCapture = "perry_roots_capture" (I32) -> I32;
    RootsReserve = "perry_roots_reserve" (I32);
    RootsState = "perry_roots_state" () -> I32;
    Status = "perry_status" () -> I32;
    StringScalarConcat = "perry_string_scalar_concat" (I32, I32) -> I32;
    StringScalarAppend = "perry_string_scalar_append" (I32, I32) -> I32;
    StdStringScalarAt = "std_string_scalar_at" (I32, F64) -> I32;
    StringAppend = "perry_string_append" (I32, I32) -> I32;
    TaskCancel = "perry_task_cancel" () -> I32;
    TaskCurrent = "perry_task_current" () -> I32;
    TaskDetached = "perry_task_detached" (I32, I32);
    StdBigintCompare = "std_bigint_compare" (I32, I32) -> I32;
    StdBigintFromNumber = "std_bigint_from_number" (F64) -> I32;
    StdBigintNegate = "std_bigint_negate" (I32) -> I32;
    StdBigintToNumber = "std_bigint_to_number" (I32) -> F64;
    StdBigintToString = "std_bigint_to_string" (I32) -> I32;
    StdBufferLength = "std_buffer_length" (I32) -> F64;
    StdBytesAdopt = "std_bytes_adopt" (I32, I32) -> I32;
    StdBytesBoundary = "std_bytes_boundary" (I32) -> I32;
    StdBytesBuffer = "std_bytes_buffer" (I32) -> I32;
    StdBytesCopy = "std_bytes_copy" (I32) -> I32;
    StdBytesFill = "std_bytes_fill" (I32, F64, F64, F64);
    StdBytesFillRandom = "std_bytes_fill_random" (I32) -> I32;
    StdBytesFromArray = "std_bytes_from_array" (I32) -> I32;
    StdBytesFromBuffer = "std_bytes_from_buffer" (I32) -> I32;
    StdBytesGet = "std_bytes_get" (I32, F64) -> I64;
    StdBytesLength = "std_bytes_length" (I32) -> F64;
    StdBytesNew = "std_bytes_new" (F64) -> I32;
    StdBytesOffset = "std_bytes_offset" (I32) -> F64;
    StdBytesSet = "std_bytes_set" (I32, F64, F64);
    StdBytesSetFrom = "std_bytes_set_from" (I32, I32, F64);
    StdBytesSlice = "std_bytes_slice" (I32, F64, F64) -> I32;
    StdBytesSubarray = "std_bytes_subarray" (I32, F64, F64) -> I32;
    StdDecoderDecode = "std_decoder_decode" (I32, I32, I32) -> I32;
    StdDecoderNew = "std_decoder_new" (I32, I32) -> I32;
    StdExceptionName = "std_exception_name" (I32) -> I32;
    StdExceptionToNumber = "std_exception_to_number" (I32) -> F64;
    StdExceptionToString = "std_exception_to_string" (I32) -> I32;
    StdJsonAssign = "std_json_assign" (I32, I32);
    StdJsonClose = "std_json_close" (I32, I32);
    StdJsonFinish = "std_json_finish" (I32) -> I32;
    StdJsonFirst = "std_json_first" (I32) -> I32;
    StdJsonFromNumber = "std_json_from_number" (F64) -> I32;
    StdJsonFromScalar = "std_json_from_scalar" (I32, I32) -> I32;
    StdJsonFromString = "std_json_from_string" (I32) -> I32;
    StdJsonIndex = "std_json_index" (I32, F64) -> I32;
    StdJsonIsNull = "std_json_is_null" (I32) -> I32;
    StdJsonItem = "std_json_item" (I32);
    StdJsonKey = "std_json_key" (I32, I32);
    StdJsonKeys = "std_json_keys" (I32) -> I32;
    StdJsonLength = "std_json_length" (I32) -> I64;
    StdJsonMember = "std_json_member" (I32, I32) -> I32;
    StdJsonNext = "std_json_next" (I32) -> I32;
    StdJsonObject = "std_json_object" () -> I32;
    StdJsonOpen = "std_json_open" (I32, I32) -> I32;
    StdJsonQuote = "std_json_quote" (I32, I32);
    StdJsonRaw = "std_json_raw" (I32, I32, I32);
    StdJsonNumber = "std_json_number" (I32, F64);
    StdJsonReadArray = "std_json_read_array" (I32) -> I32;
    StdJsonReadVariant = "std_json_read_variant" (I32, I32) -> I32;
    StdJsonReadPayload = "std_json_read_payload" (I32) -> I32;
    StdJsonReadResume = "std_json_read_resume" (I32, I32);
    StdJsonReadPosition = "std_json_read_position" (I32) -> I32;
    StdJsonReadAttach = "std_json_read_attach" (I32, I32, I32, I32, I32);
    StdJsonSourceTupleLength = "std_json_source_tuple_length" (I32, I32) -> I32;
    StdJsonSourceTupleTail = "std_json_source_tuple_tail" (I32, I32, I32);
    StdJsonSourceCopy = "std_json_source_copy" (I32, I32) -> I32;
    StdJsonSourceRemember = "std_json_source_remember" (I32, I32);
    StdJsonSourceWrite = "std_json_source_write" (I32, I32);
    StdJsonSourceBegin = "std_json_source_begin" (I32) -> I32;
    StdJsonSourceNext = "std_json_source_next" (I32, I32, I32, I32) -> I32;
    StdJsonReadBegin = "std_json_read_begin" (I32) -> I32;
    StdJsonReadBool = "std_json_read_bool" (I32) -> I32;
    StdJsonReadElement = "std_json_read_element" (I32) -> I32;
    StdJsonReadEnd = "std_json_read_end" (I32);
    StdJsonReadTakeError = "std_json_read_take_error" (I32) -> I32;
    StdJsonReadFail = "std_json_read_fail" (I32, I32);
    StdJsonReadField = "std_json_read_field" (I32, I32, I32) -> I32;
    StdJsonReadKind = "std_json_read_kind" (I32) -> I32;
    StdJsonReadNumber = "std_json_read_number" (I32) -> F64;
    StdJsonReadObject = "std_json_read_object" (I32) -> I32;
    StdJsonReadRaw = "std_json_read_raw" (I32) -> I32;
    StdJsonReadSkip = "std_json_read_skip" (I32);
    StdJsonReadString = "std_json_read_string" (I32) -> I32;
    StdJsonRequireNumber = "std_json_require_number" (I32) -> F64;
    StdJsonRequireObject = "std_json_require_object" (I32);
    StdJsonSet = "std_json_set" (I32, I32, I32);
    StdJsonToBool = "std_json_to_bool" (I32) -> I32;
    StdJsonToNumber = "std_json_to_number" (I32) -> F64;
    StdJsonToString = "std_json_to_string" (I32) -> I32;
    StdJsonTruthy = "std_json_truthy" (I32) -> I32;
    StdJsonTypeOf = "std_json_type_of" (I32) -> I32;
    StdJsonValue = "std_json_value" (I32, I32);
    StdJsonWriter = "std_json_writer" () -> I32;
    StdMathRandom = "std_math_random" () -> F64;
    StdRaiseInvalidDate = "std_raise_invalid_date" () -> I32;
    StdRaiseSyntax = "std_raise_syntax" () -> I32;
    StdRandomUuid = "std_random_uuid" () -> I32;
    StdRegexSearch = "std_regex_search" (I32, I32) -> F64;
    StdStringByteInput = "std_string_byte_input" (I32) -> I32;
    StdAsciiStrings = "std_ascii_strings" () -> I32;
    StdStringCharAt = "std_string_char_at" (I32, F64) -> I32;
    StdStringCodePointAt = "std_string_code_point_at" (I32, F64) -> F64;
    StdStringEndsWith = "std_string_ends_with" (I32, I32, F64) -> I32;
    StdStringFromCodePoint = "std_string_from_code_point" (F64) -> I32;
    StdStringIndex = "std_string_index" (I32, F64) -> I32;
    StdStringIndexOf = "std_string_index_of" (I32, I32, F64) -> F64;
    StdStringLastIndexOf = "std_string_last_index_of" (I32, I32, F64) -> F64;
    StdStringRepeat = "std_string_repeat" (I32, F64) -> I32;
    StdStringSlice = "std_string_slice" (I32, F64, F64) -> I32;
    StdStringSplit = "std_string_split" (I32, I32) -> I32;
    StdStringStartsWith = "std_string_starts_with" (I32, I32, F64) -> I32;
    StdStringSubstring = "std_string_substring" (I32, F64, F64) -> I32;
    StdStringTrim = "std_string_trim" (I32) -> I32;
    StdTextEncode = "std_text_encode" (I32) -> I32;
    StrCaseConvert = "str_case_convert" (I32, I32, I32, I32) -> I64;
    TimeDateIso = "time_date_iso" (F64, I32, I32) -> I64;
    TimeDateParse = "time_date_parse" (I32, I32) -> F64;
    TimeDatePart = "time_date_part" (F64, I32) -> F64;
    TimeDateSet = "time_date_set" (F64, I32, I32, F64, F64, F64, F64) -> F64;
    TimeDateUtc = "time_date_utc" (F64, F64, F64, F64, F64, F64, F64) -> F64;
    TimeInstantFormat = "time_instant_format" (I32, I32, I32) -> I64;
    TimeInstantFromMs = "time_instant_from_ms" (F64, I32) -> I64;
    TimeInstantMs = "time_instant_ms" (I32) -> F64;
    TimeInstantParse = "time_instant_parse" (I32, I32, I32) -> I64;
    TimePlainAddDays = "time_plain_add_days" (I32, F64, I32) -> I64;
    TimePlainDateFormat = "time_plain_date_format" (I32, I32, I32) -> I64;
    TimePlainDateParse = "time_plain_date_parse" (I32, I32, I32) -> I64;
    TimePlainFormat = "time_plain_format" (I32, I32, I32) -> I64;
    TimePlainParse = "time_plain_parse" (I32, I32, I32) -> I64;
    TimePlainPart = "time_plain_part" (I32, I32) -> F64;
    WebBodyBytes = "web_body_bytes" (I32) -> I32;
    WebBodyJson = "web_body_json" (I32) -> I32;
    WebBodyStream = "web_body_stream" (I32) -> I32;
    WebBodyText = "web_body_text" (I32) -> I32;
    WebBodyUsed = "web_body_used" (I32) -> I32;
    WebEncodeUriComponent = "web_encode_uri_component" (I32) -> I32;
    WebEvent = "web_event" (I32, I32, I32) -> I32;
    WebFetch = "web_fetch" (I32, I32, I32, I32, I32) -> I32;
    WebFetchClose = "web_fetch_close" ();
    WebFetchEvent = "web_fetch_event" (I32, I32, I32) -> I32;
    WebFetchPending = "web_fetch_pending" () -> I32;
    WebHeadersAppend = "web_headers_append" (I32, I32, I32);
    WebHeadersCopy = "web_headers_copy" (I32) -> I32;
    WebHeadersDelete = "web_headers_delete" (I32, I32);
    WebHeadersGet = "web_headers_get" (I32, I32) -> I32;
    WebHeadersHas = "web_headers_has" (I32, I32) -> I32;
    WebHeadersNew = "web_headers_new" () -> I32;
    WebHeadersSet = "web_headers_set" (I32, I32, I32);
    WebJsonSettle = "web_json_settle" ();
    WebReadDone = "web_read_done" (I32) -> I32;
    WebReadValue = "web_read_value" (I32) -> I32;
    WebReadableCancel = "web_readable_cancel" (I32) -> I32;
    WebReadableGetReader = "web_readable_get_reader" (I32) -> I32;
    WebReadableLocked = "web_readable_locked" (I32) -> I32;
    WebReadableNew = "web_readable_new" (I32) -> I32;
    WebReaderCancel = "web_reader_cancel" (I32) -> I32;
    WebReaderRead = "web_reader_read" (I32, I32) -> I32;
    WebReaderReadByob = "web_reader_read_byob" (I32, I32, F64) -> I32;
    WebReaderReleaseLock = "web_reader_release_lock" (I32);
    WebRequestHeaders = "web_request_headers" (I32) -> I32;
    WebRequestMethod = "web_request_method" (I32) -> I32;
    WebRequestNew = "web_request_new" (I32, I32, I32, I32, I32) -> I32;
    WebRequestUrl = "web_request_url" (I32) -> I32;
    WebResponseHeaders = "web_response_headers" (I32) -> I32;
    WebResponseNew = "web_response_new" (I32, I32, F64, I32, I32) -> I32;
    WebResponseOk = "web_response_ok" (I32) -> I32;
    WebResponseStatus = "web_response_status" (I32) -> F64;
    WebResponseStatusText = "web_response_status_text" (I32) -> I32;
    WebServeAddHeader = "web_serve_add_header" (I32, I32, I32);
    WebServeAuthority = "web_serve_authority" (I32) -> I32;
    WebServeBody = "web_serve_body" (I32) -> I32;
    WebServeBusy = "web_serve_busy" () -> I32;
    WebServeClose = "web_serve_close" ();
    WebServeEvent = "web_serve_event" (I32, I32, I32) -> I32;
    WebServeHasScheme = "web_serve_has_scheme" (I32) -> I32;
    WebServeHeaderCount = "web_serve_header_count" (I32) -> F64;
    WebServeHeaderName = "web_serve_header_name" (I32, F64) -> I32;
    WebServeHeaderValue = "web_serve_header_value" (I32, F64) -> I32;
    WebServeMarkReturned = "web_serve_mark_returned" ();
    WebServeMethod = "web_serve_method" (I32) -> I32;
    WebServeMethodOther = "web_serve_method_other" (I32) -> I32;
    WebServePath = "web_serve_path" (I32) -> I32;
    WebServeRequest = "web_serve_request" (F64) -> I32;
    WebServeRespond = "web_serve_respond" (I32, F64, I32) -> F64;
    WebServeReturned = "web_serve_returned" () -> I32;
    WebServeScheme = "web_serve_scheme" (I32) -> I32;
    WebServeSchemeOther = "web_serve_scheme_other" (I32) -> I32;
    WebStreamsClose = "web_streams_close" ();
    WebStreamsPending = "web_streams_pending" () -> I32;
    WebUrlAppend = "web_url_append" (I32, I32, I32);
    WebUrlGet = "web_url_get" (I32, F64) -> I32;
    WebUrlNew = "web_url_new" (I32, I32) -> I32;
    WebUrlSetPathname = "web_url_set_pathname" (I32, I32);
    WebWritableGetWriter = "web_writable_get_writer" (I32) -> I32;
    WebWritableLocked = "web_writable_locked" (I32) -> I32;
    WebWritableNew = "web_writable_new" (I32) -> I32;
    WebWriterClose = "web_writer_close" (I32) -> I32;
    WebWriterReleaseLock = "web_writer_release_lock" (I32);
    WebWriterWrite = "web_writer_write" (I32, I32, I32) -> I32;
}

impl Runtime {
    /// The symbol of the import in the compiled module.
    pub(crate) fn symbol(self) -> Identifier {
        let name = self.name();
        format!("rt_{}", name.strip_prefix("perry_").unwrap_or(name))
            .try_into()
            .expect("runtime names are identifiers")
    }

    /// MIR parameter and result types of the import.
    pub(crate) fn signature(self, ctx: &mut Context) -> (Vec<TypeHandle>, Vec<TypeHandle>) {
        let params = self.params().iter().map(|w| w.mir(ctx)).collect();
        let results = self.result().map(|w| w.mir(ctx)).into_iter().collect();
        (params, results)
    }
}
