//! Runtime and helper-library functions that compiled code imports, each declared once
//! with its Wasm signature. Calls name a [Runtime] variant; the import is emitted once per
//! module for every function used.
use perry_mir::Intrinsic;
use pliron::{
    builtin::types::{FP64Type, IntegerType, Signedness},
    context::Context,
    identifier::Identifier,
    r#type::TypeHandle,
};

/// Runtime ABI carriers; `F64Bits` preserves optional-number sentinels through an i64.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum W {
    I32,
    I64,
    F64,
    F64Bits,
}

impl W {
    fn mir(self, ctx: &mut Context) -> TypeHandle {
        match self {
            W::I32 => IntegerType::get(ctx, 32, Signedness::Signless).into(),
            W::I64 | W::F64Bits => IntegerType::get(ctx, 64, Signedness::Signless).into(),
            W::F64 => FP64Type::get(ctx).into(),
        }
    }
}

macro_rules! runtime_functions {
    ($($variant:ident = $name:literal ($($param:ident),*) $(-> $result:ident)? $(for $($intrinsic:ident)|+)?;)*) => {
        /// A function of the runtime or of a helper library.
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
        pub(crate) enum Runtime {
            $($variant,)*
        }

        impl Runtime {
            pub(crate) fn for_intrinsic(intrinsic: Intrinsic) -> Option<Self> {
                match intrinsic {
                    $($( $(Intrinsic::$intrinsic)|+ => Some(Self::$variant), )?)*
                    _ => None,
                }
            }

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
    ArrayConcatSegments = "perry_array_concat_segments" (I32, I32) -> I32;
    ArrayConcatMany = "perry_array_concat_many" (I32, I32, I32, I32, I32) -> I32;
    ArrayExtend = "perry_array_extend" (I32, I32, I32);
    ArrayConcat = "perry_array_concat" (I32, I32, I32, I32) -> I32;
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
    ExceptionImplicit = "perry_exception_implicit" (I32, I32) -> I32;
    ExceptionRaise = "perry_exception_raise" (I32) -> I32;
    ExceptionCatch = "perry_exception_catch" (I32) -> I32;
    ExceptionMessage = "perry_exception_message" (I32) -> I32;
    Exit = "perry_exit" (F64) -> I32;
    StringAdopt = "perry_string_adopt" (I32, I32) -> I32;
    StringAdoptCounted = "perry_string_adopt_counted" (I32, I32, I32) -> I32;
    StringConcat = "perry_string_concat" (I32, I32) -> I32;
    StringCompare = "perry_string_compare" (I32, I32) -> I32;
    Print = "perry_print" (I32, I32, I32);
    IoPump = "perry_io_pump" ();
    IoEvent = "perry_io_event" (I32, I32, I32) -> I32;
    IoClose = "perry_io_close" () -> I32;
    ExitCodeSet = "perry_exit_code_set" (F64) for ExitCodeSet;
    ReportRejection = "perry_report_rejection" (I32);
    CommandOutcome = "perry_command_outcome" (I32) -> I32;
    RejectionText = "perry_rejection_text" (I32) -> I32;
    AbortNew = "perry_abort_new" () -> I32 for AbortNew;
    Abort = "perry_abort" (I32, I32, I32);
    AbortAborted = "perry_abort_aborted" (I32) -> I32 for AbortAborted;
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
    FsStatsIsDirectory = "fs_stats_is_directory" (I32) -> I32 for StatsIsDirectory;
    FsStatsIsFile = "fs_stats_is_file" (I32) -> I32 for StatsIsFile;
    FsStatsSize = "fs_stats_size" (I32) -> F64 for StatsSize;
    FsUnlink = "fs_unlink" (I32) -> I32;
    FsWriteBytes = "fs_write_bytes" (I32, I32) -> I32;
    FsWriteText = "fs_write_text" (I32, I32) -> I32;
    JsonCursorBegin = "json_cursor_begin" (I32) -> I32 for JsonCursorBegin;
    JsonCursorByte = "json_cursor_byte" (I32) -> I32 for JsonCursorByte;
    JsonCursorConsume = "json_cursor_consume" (I32, I32) for JsonCursorConsume;
    JsonCursorNumber = "json_cursor_number" (I32) -> F64 for JsonCursorNumber;
    JsonCursorI64 = "json_cursor_i64" (I32) -> I64 for JsonCursorI64;
    JsonCursorU64 = "json_cursor_u64" (I32) -> I64 for JsonCursorU64;
    JsonCursorFailed = "json_cursor_failed" (I32) -> I32 for JsonCursorFailed;
    JsonCursorBoolean = "json_cursor_boolean" (I32) -> I32 for JsonCursorBoolean;
    JsonCursorString = "json_cursor_string" (I32) -> I32 for JsonCursorString;
    JsonCursorDiscard = "json_cursor_discard" (I32) for JsonCursorDiscard;
    JsonCursorValue = "json_cursor_value" (I32) -> I32 for JsonCursorValue;
    JsonCursorMember = "json_cursor_member" (I32, I32) -> I32 for JsonCursorMember;
    JsonCursorExtra = "json_cursor_extra" (I32) for JsonCursorExtra;
    JsonCursorRecord = "json_cursor_record" (I32, I32) for JsonCursorRecord;
    JsonCursorTail = "json_cursor_tail" (I32, I32, I32) -> I32 for JsonCursorTail;
    JsonCursorSeek = "json_cursor_seek" (I32, I32) -> I32 for JsonCursorSeek;
    JsonCursorFinish = "json_cursor_finish" (I32, I32) for JsonCursorFinish;
    JsonCursorKey = "json_cursor_key" (I32, I32, I32, I32) -> I32;
    JsonParseString = "json_parse_string" (I32) -> I32;
    ArrayGrowCollectible = "perry_array_grow_collectible" (I32, I32);
    ArrayNewCollectible = "perry_array_new_collectible" (I32, I32) -> I32;
    ArrayGrowTyped = "perry_array_grow_typed" (I32, I32);
    ArrayNewTyped = "perry_array_new_typed" (I32, I32) -> I32;
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
    StringScalarConcat = "perry_string_scalar_concat" (I32, I32) -> I32 for StringScalarConcat;
    StringScalarAppend = "perry_string_scalar_append" (I32, I32) -> I32 for StringScalarAppend;
    StdStringScalarAt = "std_string_scalar_at" (I32, F64) -> I32;
    StringAppend = "perry_string_append" (I32, I32) -> I32;
    TaskCancel = "perry_task_cancel" () -> I32;
    TaskCurrent = "perry_task_current" () -> I32;
    TaskDetached = "perry_task_detached" (I32, I32);
    StdBigintCompare = "std_bigint_compare" (I32, I32) -> I32;
    StdBigintFromNumber = "std_bigint_from_number" (F64) -> I32 for BigIntFromNumber;
    StdBigintNegate = "std_bigint_negate" (I32) -> I32 for BigIntNegate;
    StdBigintToNumber = "std_bigint_to_number" (I32) -> F64 for BigIntToNumber;
    StdBigintToString = "std_bigint_to_string" (I32) -> I32 for BigIntToString;
    StdBufferLength = "std_buffer_length" (I32) -> F64 for BufferLength;
    StdBytesAdopt = "std_bytes_adopt" (I32, I32) -> I32;
    StdBytesBoundary = "std_bytes_boundary" (I32) -> I32;
    StdBytesBuffer = "std_bytes_buffer" (I32) -> I32 for BytesBuffer;
    StdBytesCopy = "std_bytes_copy" (I32) -> I32 for BytesCopy;
    StdBytesFill = "std_bytes_fill" (I32, F64, F64, F64) for BytesFill;
    StdBytesFillRandom = "std_bytes_fill_random" (I32) -> I32;
    StdBytesFromArray = "std_bytes_from_array" (I32) -> I32 for BytesFromArray;
    StdBytesFromBuffer = "std_bytes_from_buffer" (I32) -> I32 for BytesFromBuffer;
    StdBytesGet = "std_bytes_get" (I32, F64) -> F64Bits for BytesGet;
    StdBytesLength = "std_bytes_length" (I32) -> F64 for BytesLength;
    StdBytesNew = "std_bytes_new" (F64) -> I32 for BytesNew;
    StdBytesOffset = "std_bytes_offset" (I32) -> F64 for BytesOffset;
    StdBytesSet = "std_bytes_set" (I32, F64, F64) for BytesSet;
    StdBytesSetFrom = "std_bytes_set_from" (I32, I32, F64) for BytesSetFrom;
    StdBytesSlice = "std_bytes_slice" (I32, F64, F64) -> I32 for BytesSlice;
    StdBytesSubarray = "std_bytes_subarray" (I32, F64, F64) -> I32 for BytesSubarray;
    StdDecoderDecode = "std_decoder_decode" (I32, I32, I32) -> I32 for TextDecode;
    StdDecoderValidateLabel = "std_decoder_validate_label" (I32) for DecoderValidateLabel;
    StdDecoderNew = "std_decoder_new" (I32, I32) -> I32 for DecoderNew;
    StdExceptionName = "std_exception_name" (I32) -> I32 for ExceptionName;
    StdExceptionEqual = "std_exception_equal" (I32, I32) -> I32 for ExceptionEqual;
    StdExceptionBigIntEqual = "std_exception_bigint_equal" (I32, I32) -> I32 for ExceptionBigIntEqual;
    StdExceptionToBool = "std_exception_to_bool" (I32) -> I32 for ExceptionToBool;
    StdExceptionToNumber = "std_exception_to_number" (I32) -> F64 for ExceptionToNumber;
    StdExceptionToString = "std_exception_to_string" (I32) -> I32 for ExceptionToString;
    StdJsonAssign = "std_json_assign" (I32, I32) for JsonAssign;
    StdJsonClose = "std_json_close" (I32, I32);
    StdJsonFinish = "std_json_finish" (I32) -> I32;
    StdJsonExtras = "std_json_extras" (I32, I32, I32, I32);
    StdJsonMembers = "std_json_members" (I32) -> I32;
    StdJsonRecordIndex = "std_json_record_index" (I32, I32, I32) -> I32;
    StdJsonRecordRetain = "std_json_record_retain" (I32, I32, I32, I32);
    StdJsonRecordSpread = "std_json_record_spread" (I32, I32, I32, I32, I32, I32);
    StdJsonRecordField = "std_json_record_field" (I32, I32, I32, I32, I32);
    StdJsonWriteMember = "std_json_write_member" (I32, I32);
    StdJsonFirst = "std_json_first" (I32) -> I32 for JsonFirst;
    StdJsonNative = "std_json_native" (I32, I32) -> I32;
    StdJsonStrictEqual = "std_json_strict_equal" (I32, I32) -> I32 for JsonStrictEqual;
    StdJsonExceptionEqual = "std_json_exception_equal" (I32, I32) -> I32 for JsonExceptionEqual;
    StdJsonOriginEqual = "std_json_origin_equal" (I32, I32) -> I32 for JsonOriginEqual;
    StdJsonFromException = "std_json_from_exception" (I32) -> I32 for JsonFromException;
    StdJsonToException = "std_json_to_exception" (I32) -> I32 for JsonToException;
    StdJsonExceptionIs = "std_json_exception_is" (I32, I32) -> I32 for JsonExceptionIs;
    StdJsonFromNumber = "std_json_from_number" (F64) -> I32 for JsonFromNumber;
    StdJsonFromScalar = "std_json_from_scalar" (I32, I32) -> I32;
    StdJsonFromString = "std_json_from_string" (I32) -> I32 for JsonFromString;
    StdJsonIndex = "std_json_index" (I32, F64) -> I32 for JsonIndex;
    StdJsonIsNull = "std_json_is_null" (I32) -> I32 for JsonIsNull;
    StdJsonItem = "std_json_item" (I32);
    StdJsonKey = "std_json_key" (I32, I32);
    StdJsonKeys = "std_json_keys" (I32) -> I32 for JsonKeys;
    StdJsonLength = "std_json_length" (I32) -> F64Bits for JsonLength;
    StdJsonMember = "std_json_member" (I32, I32) -> I32 for JsonMember;
    StdJsonNext = "std_json_next" (I32) -> I32 for JsonNext;
    StdJsonObject = "std_json_object" () -> I32 for JsonObject;
    StdJsonOpen = "std_json_open" (I32, I32) -> I32;
    StdJsonQuote = "std_json_quote" (I32, I32);
    StdJsonRaw = "std_json_raw" (I32, I32, I32);
    StdJsonNumber = "std_json_number" (I32, F64);
    StdJsonRequireNumber = "std_json_require_number" (I32) -> F64 for JsonRequireNumber;
    StdJsonRequireObject = "std_json_require_object" (I32) for JsonRequireObject;
    StdJsonSet = "std_json_set" (I32, I32, I32) for JsonSet;
    StdJsonToBool = "std_json_to_bool" (I32) -> I32 for JsonToBool;
    StdJsonToNumber = "std_json_to_number" (I32) -> F64 for JsonToNumber;
    StdJsonToI64 = "std_json_to_i64" (I32) -> I64 for JsonToI64;
    StdJsonToU64 = "std_json_to_u64" (I32) -> I64 for JsonToU64;
    StdJsonToString = "std_json_to_string" (I32) -> I32 for JsonToString;
    StdJsonTruthy = "std_json_truthy" (I32) -> I32 for JsonTruthy;
    StdJsonDisplay = "std_json_display" (I32) -> I32 for JsonDisplay;
    StdJsonTypeOf = "std_json_type_of" (I32) -> I32 for JsonTypeOf;
    StdJsonValue = "std_json_value" (I32, I32);
    StdJsonWriteNumeric = "std_json_write_numeric" (I32, I32, I64);
    StdJsonWriter = "std_json_writer" () -> I32;
    StdMathRandom = "std_math_random" () -> F64;
    StdNumberToString = "std_number_to_string" (I32, I64) -> I32;
    StdNumberInspect = "std_number_inspect" (I32, I64) -> I32;
    StdNumberToStringRadix = "std_number_to_string_radix" (I32, I64, F64) -> I32;
    StdParseFloat = "std_parse_float" (I32) -> F64 for ParseFloat;
    StdParseInt = "std_parse_int" (I32, I32, I64, I32) -> I32;
    StdRaiseInvalidDate = "std_raise_invalid_date" () -> I32;
    StdRandomUuid = "std_random_uuid" () -> I32;
    StdRegexExec = "std_regex_exec" (I32, I32) -> I32 for RegexExec;
    StdRegexMatch = "std_regex_match" (I32, I32) -> I32 for RegexMatch;
    StdRegexReplace = "std_regex_replace" (I32, I32, I32) -> I32 for RegexReplace;
    StdRegexSearch = "std_regex_search" (I32, I32) -> F64 for RegexSearch;
    StdRegexSplit = "std_regex_split" (I32, I32) -> I32 for RegexSplit;
    StdStringByteInput = "std_string_byte_input" (I32) -> I32 for StringByteInput;
    StdAsciiStrings = "std_ascii_strings" () -> I32;
    StdStringAt = "std_string_at" (I32, F64) -> I32 for StringAt;
    StdStringCharAt = "std_string_char_at" (I32, F64) -> I32;
    StdStringCodePointAt = "std_string_code_point_at" (I32, F64) -> F64 for StringCodePointAt;
    StdStringEndsWith = "std_string_ends_with" (I32, I32, F64) -> I32 for StringEndsWith;
    StdStringFromCodePoint = "std_string_from_code_point" (F64) -> I32 for StringFromCodePoint;
    StdStringIndex = "std_string_index" (I32, F64) -> I32 for StringIndex;
    StdStringIndexOf = "std_string_index_of" (I32, I32, F64) -> F64 for StringIndexOf;
    StdStringLastIndexOf = "std_string_last_index_of" (I32, I32, F64) -> F64 for StringLastIndexOf;
    StdStringPadEnd = "std_string_pad_end" (I32, F64, I32) -> I32 for StringPadEnd;
    StdStringPadStart = "std_string_pad_start" (I32, F64, I32) -> I32 for StringPadStart;
    StdStringNormalize = "std_string_normalize" (I32, I32) -> I32 for StringNormalize;
    StdStringRepeat = "std_string_repeat" (I32, F64) -> I32 for StringRepeat;
    StdStringReplace = "std_string_replace" (I32, I32, I32) -> I32 for StringReplace;
    StdStringReplaceAll = "std_string_replace_all" (I32, I32, I32) -> I32 for StringReplaceAll;
    StdStringSlice = "std_string_slice" (I32, F64, F64) -> I32 for StringSlice;
    StdStringSplit = "std_string_split" (I32, I32) -> I32 for StringSplit;
    StdStringStartsWith = "std_string_starts_with" (I32, I32, F64) -> I32 for StringStartsWith;
    StdStringSubstring = "std_string_substring" (I32, F64, F64) -> I32 for StringSubstring;
    StdStringToNumber = "std_string_to_number" (I32) -> F64 for StringToNumber;
    StdStringTrim = "std_string_trim" (I32) -> I32 for StringTrim;
    StdStringTrimEnd = "std_string_trim_end" (I32) -> I32 for StringTrimEnd;
    StdStringTrimStart = "std_string_trim_start" (I32) -> I32 for StringTrimStart;
    StdTextEncode = "std_text_encode" (I32) -> I32 for TextEncode;
    StrCaseConvert = "str_case_convert" (I32, I32, I32, I32) -> I64;
    TimeDateIso = "time_date_iso" (F64, I32, I32) -> I64;
    TimeDateParse = "time_date_parse" (I32, I32) -> F64;
    TimeDatePart = "time_date_part" (F64, I32) -> F64;
    TimeDateSet = "time_date_set" (F64, I32, I32, F64, F64, F64, F64) -> F64;
    TimeDateUtc = "time_date_utc" (F64, F64, F64, F64, F64, F64, F64) -> F64;
    TimeInstantFormat = "time_instant_format" (I32, I32, I32) -> I64;
    TimeInstantFromMs = "time_instant_from_ms" (F64, I32) -> I64;
    TimeInstantMs = "time_instant_ms" (I32, I32) -> I64;
    TimeInstantParse = "time_instant_parse" (I32, I32, I32) -> I64;
    TimePlainAddCalendar = "time_plain_add_calendar" (I32, F64, F64, F64, F64, I32, I32) -> I64;
    TimePlainAddDays = "time_plain_add_days" (I32, F64, I32) -> I64;
    TimePlainDateFormat = "time_plain_date_format" (I32, I32, I32) -> I64;
    TimePlainDateFromFields = "time_plain_date_from_fields" (F64, F64, F64, I32, I32) -> I64;
    TimePlainDateParse = "time_plain_date_parse" (I32, I32, I32) -> I64;
    TimePlainFormat = "time_plain_format" (I32, I32, I32) -> I64;
    TimePlainParse = "time_plain_parse" (I32, I32, I32) -> I64;
    TimePlainPart = "time_plain_part" (I32, I32, I32) -> I64;
    TimePlainUntilPart = "time_plain_until_part" (I32, I32, I32, I32, I32) -> I64;
    WebBodyBytes = "web_body_bytes" (I32) -> I32 for ResponseBytes | RequestBytes;
    WebBodyJson = "web_body_json" (I32) -> I32;
    WebBodyStream = "web_body_stream" (I32) -> I32;
    WebBodyText = "web_body_text" (I32) -> I32 for ResponseText | RequestText;
    WebBodyUsed = "web_body_used" (I32) -> I32 for ResponseBodyUsed | RequestBodyUsed;
    WebEncodeUriComponent = "web_encode_uri_component" (I32) -> I32 for EncodeUriComponent;
    WebEvent = "web_event" (I32, I32, I32) -> I32;
    WebFetch = "web_fetch" (I32, I32, I32, I32, I32) -> I32;
    WebFetchClose = "web_fetch_close" ();
    WebFetchEvent = "web_fetch_event" (I32, I32, I32) -> I32;
    WebFetchPending = "web_fetch_pending" () -> I32;
    WebHeadersAppend = "web_headers_append" (I32, I32, I32) for HeadersAppend;
    WebHeadersCopy = "web_headers_copy" (I32) -> I32 for HeadersCopy;
    WebHeadersDelete = "web_headers_delete" (I32, I32) for HeadersDelete;
    WebHeadersGet = "web_headers_get" (I32, I32) -> I32 for HeadersGet;
    WebHeadersHas = "web_headers_has" (I32, I32) -> I32 for HeadersHas;
    WebHeadersNew = "web_headers_new" () -> I32 for HeadersNew;
    WebHeadersSet = "web_headers_set" (I32, I32, I32) for HeadersSet;
    WebJsonSettle = "web_json_settle" ();
    WebReadDone = "web_read_done" (I32) -> I32 for ReadDone;
    WebReadValue = "web_read_value" (I32) -> I32 for ReadValue;
    WebReadableCancel = "web_readable_cancel" (I32) -> I32 for ReadableCancel;
    WebReadableGetReader = "web_readable_get_reader" (I32) -> I32 for ReadableGetReader | ReadableGetByobReader;
    WebReadableLocked = "web_readable_locked" (I32) -> I32 for ReadableLocked;
    WebReadableNew = "web_readable_new" (I32) -> I32;
    WebReaderCancel = "web_reader_cancel" (I32) -> I32 for ReaderCancel;
    WebReaderRead = "web_reader_read" (I32, I32) -> I32;
    WebReaderReadByob = "web_reader_read_byob" (I32, I32, F64) -> I32;
    WebReaderReleaseLock = "web_reader_release_lock" (I32) for ReaderReleaseLock;
    WebRequestHeaders = "web_request_headers" (I32) -> I32 for RequestHeaders;
    WebRequestMethod = "web_request_method" (I32) -> I32 for RequestMethod;
    WebRequestNew = "web_request_new" (I32, I32, I32, I32, I32) -> I32 for RequestNew;
    WebRequestUrl = "web_request_url" (I32) -> I32 for RequestUrl;
    WebResponseHeaders = "web_response_headers" (I32) -> I32 for ResponseHeaders;
    WebResponseNew = "web_response_new" (I32, I32, F64, I32, I32) -> I32 for ResponseNew;
    WebResponseOk = "web_response_ok" (I32) -> I32 for ResponseOk;
    WebResponseStatus = "web_response_status" (I32) -> F64 for ResponseStatus;
    WebResponseStatusText = "web_response_status_text" (I32) -> I32 for ResponseStatusText;
    WebServeAddHeader = "web_serve_add_header" (I32, I32, I32) for ServeAddHeader;
    WebServeAuthority = "web_serve_authority" (I32) -> I32 for ServeAuthority;
    WebServeBody = "web_serve_body" (I32) -> I32 for ServeBody;
    WebServeBusy = "web_serve_busy" () -> I32;
    WebServeClose = "web_serve_close" ();
    WebServeEvent = "web_serve_event" (I32, I32, I32) -> I32;
    WebServeHasScheme = "web_serve_has_scheme" (I32) -> I32 for ServeHasScheme;
    WebServeHeaderCount = "web_serve_header_count" (I32) -> F64 for ServeHeaderCount;
    WebServeHeaderName = "web_serve_header_name" (I32, F64) -> I32 for ServeHeaderName;
    WebServeHeaderValue = "web_serve_header_value" (I32, F64) -> I32 for ServeHeaderValue;
    WebServeMarkReturned = "web_serve_mark_returned" ();
    WebServeMethod = "web_serve_method" (I32) -> I32 for ServeMethod;
    WebServeMethodOther = "web_serve_method_other" (I32) -> I32 for ServeMethodOther;
    WebServePath = "web_serve_path" (I32) -> I32 for ServePath;
    WebServeRequest = "web_serve_request" (F64) -> I32;
    WebServeRespond = "web_serve_respond" (I32, F64, I32) -> F64 for ServeRespond;
    WebServeReturned = "web_serve_returned" () -> I32;
    WebServeScheme = "web_serve_scheme" (I32) -> I32 for ServeScheme;
    WebServeSchemeOther = "web_serve_scheme_other" (I32) -> I32 for ServeSchemeOther;
    WebStreamsClose = "web_streams_close" ();
    WebStreamsPending = "web_streams_pending" () -> I32;
    WebUrlAppend = "web_url_append" (I32, I32, I32) for UrlAppend;
    WebUrlGet = "web_url_get" (I32, F64) -> I32 for UrlGet;
    WebUrlNew = "web_url_new" (I32, I32) -> I32 for UrlNew;
    WebUrlSetPathname = "web_url_set_pathname" (I32, I32) for UrlSetPathname;
    WebWritableGetWriter = "web_writable_get_writer" (I32) -> I32 for WritableGetWriter;
    WebWritableLocked = "web_writable_locked" (I32) -> I32 for WritableLocked;
    WebWritableNew = "web_writable_new" (I32) -> I32;
    WebWriterClose = "web_writer_close" (I32) -> I32 for WriterClose;
    WebWriterReleaseLock = "web_writer_release_lock" (I32) for WriterReleaseLock;
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
