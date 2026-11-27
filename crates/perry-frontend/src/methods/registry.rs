/// Built-in methods, one row each: variant, source names, optional result rule, HIR name preservation,
/// member form, arguments, lowering recipe and arity requirement. `perry-frontend` derives `Method`
/// and `Call` from the signature columns; the MIR builder derives `Call::lower` from the recipes,
/// which name its `FunctionBuilder` operations. Intrinsics supply their own result unless overridden.
#[macro_export]
macro_rules! method_catalog {
    ($consumer:ident) => {
        $consumer! {
            Receiver::Value(Kind::Array), "array" {
                ArrayPush "push" -> [Number] Push Method [args:rest] => value(push_values(args)), "accepts values";
                ArrayConcat "concat" -> Receiver Always Method [args:rest] => value(concat_values(args)), "accepts values";
                ArrayMap "map" -> Mapped Upstream Method callback(Elements) [callback] => value(array_transform(callback, Transform::Map)), "requires one static callback";
                ArrayFilter "filter" -> Receiver Upstream Method callback(Elements) [callback] => value(array_transform(callback, Transform::Filter)), "requires one static callback";
                ArrayFlatMap "flatMap" -> FlatMapped Upstream Method callback(Elements) [callback] => value(array_transform(callback, Transform::FlatMap)), "requires one static callback";
                ArrayFind "find" -> Element Upstream Method callback(AllIndices) [callback] => value(array_search(Search::Find(callback, FindResult::Value, Direction::Forward))), "requires one static callback";
                ArrayFindIndex "findIndex" -> [Number] Upstream Method callback(AllIndices) [callback] => value(array_search(Search::Find(callback, FindResult::Index, Direction::Forward))), "requires one static callback";
                ArrayFindLast "findLast" -> Element Always Method callback(AllIndices) [callback] => value(array_search(Search::Find(callback, FindResult::Value, Direction::Reverse))), "requires one static callback";
                ArrayFindLastIndex "findLastIndex" -> [Number] Always Method callback(AllIndices) [callback] => value(array_search(Search::Find(callback, FindResult::Index, Direction::Reverse))), "requires one static callback";
                ArraySome "some" -> [Bool] Upstream Method callback(Elements) [callback] => value(array_search(Search::Quantifier(callback, Quantifier::Some))), "requires one static callback";
                ArrayEvery "every" -> [Bool] Upstream Method callback(Elements) [callback] => value(array_search(Search::Quantifier(callback, Quantifier::Every))), "requires one static callback";
                ArrayForEach "forEach" -> [] Always Method callback(Elements) [callback] => value(array_for_each(callback)), "requires one static callback";
                ArraySort "sort" -> Receiver Upstream Method callback(Comparator) [comparator:optional] => value(array_sort(comparator, Mutation::InPlace)), "accepts at most one comparator callback";
                ArrayToSorted "toSorted" -> Receiver Upstream Method callback(Comparator) [comparator:optional] => value(array_sort(comparator, Mutation::Copy)), "accepts at most one comparator callback";
                ArrayReduce "reduce" -> Reduced Always Method callback(Fold) [callback, initial:optional] => value(array_reduce(callback, initial, Direction::Forward)), "requires a callback and optional initial value";
                ArrayReduceRight "reduceRight" -> Reduced Always Method callback(Fold) [callback, initial:optional] => value(array_reduce(callback, initial, Direction::Reverse)), "requires a callback and optional initial value";
                ArrayJoin "join" -> [String] Always Method [separator:optional] => value(array_join(separator)), "accepts at most one separator";
                ArraySlice "slice" -> Receiver Always Method [start:optional, end:optional] => value(array_slice(start, end)), "accepts at most two bounds";
                ArrayReverse "reverse" -> Receiver Always Method [] => value(array_reverse(Mutation::InPlace)), "takes no arguments";
                ArrayToReversed "toReversed" -> Receiver Always Method [] => value(array_reverse(Mutation::Copy)), "takes no arguments";
                ArrayFlat "flat" -> Flat Always Method [depth:optional] => value(array_flat(depth)), "accepts at most one depth argument";
                ArrayAt "at" -> At Always Method [index:optional] => value(array_at(index)), "accepts at most one index";
                ArrayPop "pop" -> Element Always Method [] => value(array_pop()), "takes no arguments";
                ArrayWith "with" -> Receiver Always Method [index, value] => value(array_with(index, value)), "requires index and value arguments";
                ArrayIncludes "includes" -> [Bool] Upstream Method [value, from:optional] => value(array_search(Search::Includes(value, from))), "requires a value and optional starting index";
                ArrayIndexOf "indexOf" -> [Number] Upstream Method [value, from:optional] => value(array_search(Search::Index(value, from, Direction::Forward))), "requires a value and optional starting index";
                ArrayLastIndexOf "lastIndexOf" -> [Number] Always Method [value, from:optional] => value(array_search(Search::Index(value, from, Direction::Reverse))), "requires a value and optional starting index";
            }
            Receiver::Collection($crate::source::CollectionKind::Map), "Map" {
                MapGet "get" -> Lookup Upstream Method [key] => value(map_get(key)), "requires one key argument";
                MapSet "set" -> Receiver Upstream Method [key, value] => value(map_set(key, value)), "requires key and value arguments";
                MapHas "has" -> [Bool] Upstream Method [key] => keyed(Map, key, kv_has), "requires one key argument";
                MapDelete "delete" -> [Bool] Upstream Method [key] => keyed(Map, key, kv_delete), "requires one key argument";
                MapClear "clear" -> [] Upstream Method [] => value(collection_clear()), "takes no arguments";
                MapForEach "forEach" -> [] Always Method callback(Collection) [callback] => value(collection_for_each(callback)), "requires one callback argument";
            }
            Receiver::Collection($crate::source::CollectionKind::Set), "Set" {
                SetAdd "add" -> Receiver Upstream Method [value] => keyed(Set, value, set_add), "requires one value argument";
                SetHas "has" -> [Bool] Upstream Method [value] => keyed(Set, value, kv_has), "requires one value argument";
                SetDelete "delete" -> [Bool] Upstream Method [value] => keyed(Set, value, kv_delete), "requires one value argument";
                SetDifference "difference" -> Receiver Always Method [other] => value(set_operation(other, SetOperation::Difference)), "requires one Set argument";
                SetIntersection "intersection" -> Receiver Always Method [other] => value(set_operation(other, SetOperation::Intersection)), "requires one Set argument";
                SetUnion "union" -> Receiver Always Method [other] => value(set_operation(other, SetOperation::Union)), "requires one Set argument";
                SetSymmetricDifference "symmetricDifference" -> Receiver Always Method [other] => value(set_operation(other, SetOperation::SymmetricDifference)), "requires one Set argument";
                SetIsSubsetOf "isSubsetOf" -> [Bool] Always Method [other] => value(set_relation(other, SetRelation::Subset)), "requires one Set argument";
                SetIsSupersetOf "isSupersetOf" -> [Bool] Always Method [other] => value(set_relation(other, SetRelation::Superset)), "requires one Set argument";
                SetIsDisjointFrom "isDisjointFrom" -> [Bool] Always Method [other] => value(set_relation(other, SetRelation::Disjoint)), "requires one Set argument";
                SetClear "clear" -> [] Upstream Method [] => value(collection_clear()), "takes no arguments";
                SetForEach "forEach" -> [] Always Method callback(Collection) [callback] => value(collection_for_each(callback)), "requires one callback argument";
            }
            Receiver::Value(Kind::String), "String" {
                StringCharAt "charAt" Upstream Method [args:rest] => intrinsic(StringCharAt, args, [Number(0.0)], Value), "accepts method arguments";
                StringCharCodeAt "charCodeAt" Upstream Method [args:rest] => intrinsic(StringCharCodeAt, args, [Number(0.0)], Value), "accepts method arguments";
                StringCodePointAt "codePointAt" Upstream Method [args:rest] => intrinsic(StringCodePointAt, args, [Number(0.0)], Value), "accepts method arguments";
                StringAt "at" -> At Always Method [args:rest] => intrinsic(StringAt, args, [Number(0.0)], Value), "accepts method arguments";
                StringPadStart "padStart" Always Method [args:rest] => intrinsic(StringPadStart, args, [Number(0.0), Padding], Value), "accepts method arguments";
                StringPadEnd "padEnd" Always Method [args:rest] => intrinsic(StringPadEnd, args, [Number(0.0), Padding], Value), "accepts method arguments";
                StringSlice "slice" Always Method [args:rest] => intrinsic(StringSlice, args, [Number(0.0), Number(f64::INFINITY)], Value), "accepts method arguments";
                StringSubstring "substring" Upstream Method [args:rest] => intrinsic(StringSubstring, args, [Number(0.0), Number(f64::INFINITY)], Value), "accepts method arguments";
                StringIndexOf "indexOf" Upstream Method [args:rest] => intrinsic(StringIndexOf, args, [SearchString, Number(0.0)], Value), "accepts method arguments";
                StringLastIndexOf "lastIndexOf" Always Method [args:rest] => intrinsic(StringLastIndexOf, args, [SearchString, Number(f64::INFINITY)], Value), "accepts method arguments";
                StringIncludes "includes" Upstream Method [args:rest] => intrinsic(StringIndexOf, args, [SearchString, Number(0.0)], Found), "accepts method arguments";
                StringStartsWith "startsWith" Upstream Method [args:rest] => intrinsic(StringStartsWith, args, [SearchString, Number(0.0)], Value), "accepts method arguments";
                StringEndsWith "endsWith" Upstream Method [args:rest] => intrinsic(StringEndsWith, args, [SearchString, Number(f64::INFINITY)], Value), "accepts method arguments";
                StringTrim "trim" Upstream Method [args:rest] => intrinsic(StringTrim, args, [], Value), "accepts method arguments";
                StringTrimStart "trimStart" | "trimLeft" Always Method [args:rest] => intrinsic(StringTrimStart, args, [], Value), "accepts method arguments";
                StringTrimEnd "trimEnd" | "trimRight" Always Method [args:rest] => intrinsic(StringTrimEnd, args, [], Value), "accepts method arguments";
                StringToLowerCase "toLowerCase" | "toLocaleLowerCase" Upstream Method [args:rest] => intrinsic(StringLower, args, [], Value), "accepts method arguments";
                StringToUpperCase "toUpperCase" | "toLocaleUpperCase" Upstream Method [args:rest] => intrinsic(StringUpper, args, [], Value), "accepts method arguments";
                StringRepeat "repeat" Upstream Method [args:rest] => intrinsic(StringRepeat, args, [Number(0.0)], Value), "accepts method arguments";
            }
            Receiver::Value(Kind::DataView), "DataView" {
                DataViewGetU32 "getUint32" Upstream Method [args:rest] => intrinsic(DataViewGetU32, args, [Number(0.0), Truthy], Value), "accepts an offset and optional endianness";
                DataViewSetU32 "setUint32" Upstream Method [args:rest] => intrinsic(DataViewSetU32, args, [Number(0.0), Number(0.0), Truthy], Undefined), "accepts an offset, value and optional endianness";
                DataViewBuffer "buffer" Upstream Property [] => intrinsic(DataViewBuffer), "takes no arguments";
                DataViewOffset "byteOffset" Upstream Property [] => intrinsic(DataViewOffset), "takes no arguments";
                DataViewLength "byteLength" Upstream Property [] => intrinsic(DataViewLength), "takes no arguments";
            }
            Receiver::Value(Kind::ByteBuffer), "ArrayBuffer" {
                BufferLength "byteLength" Upstream Property [] => intrinsic(BufferLength), "takes no arguments";
            }
            Receiver::Value(Kind::Bytes), "Bytes" {
                BytesLength "length"|"byteLength" Upstream Property [] => intrinsic(BytesLength), "takes no arguments";
                BytesOffset "byteOffset" Upstream Property [] => intrinsic(BytesOffset), "takes no arguments";
                BytesBuffer "buffer" Upstream Property [] => intrinsic(BytesBuffer), "takes no arguments";
                BytesSubarray "subarray" -> Receiver Upstream Method [args:rest] => intrinsic(BytesSubarray, args, [Number(0.0), Number(f64::INFINITY)], Value), "accepts method arguments";
                BytesSlice "slice" -> Receiver Always Method [args:rest] => intrinsic(BytesSlice, args, [Number(0.0), Number(f64::INFINITY)], Value), "accepts method arguments";
                BytesFill "fill" Upstream Method [args:rest] => intrinsic(BytesFill, args, [Number(0.0), Number(0.0), Number(f64::INFINITY)], Receiver), "accepts method arguments";
                BytesCryptoFillRandom "$$cryptoFillRandom" -> Receiver Upstream Method [args:rest] => intrinsic(BytesFillRandom, args, [], Value), "accepts method arguments";
            }
            Receiver::Value(Kind::Date), "Date" {
                DateGetTime "getTime" | "valueOf" Upstream Method [args:rest] => intrinsic(DateTime, args, [], Value), "accepts method arguments";
                DateToISOString "toISOString" Upstream Method [args:rest] => intrinsic(DateIso, args, [], Value), "accepts method arguments";
                DateToJSON "toJSON" Upstream Method [args:rest] => intrinsic(DateJson, args, [], Value), "accepts method arguments";
                DateGetFullYear "getUTCFullYear" | "getFullYear" Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(0.0)], Value), "accepts method arguments";
                DateGetMonth "getUTCMonth" | "getMonth" Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(1.0)], Value), "accepts method arguments";
                DateGetDate "getUTCDate" | "getDate" Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(2.0)], Value), "accepts method arguments";
                DateGetDay "getUTCDay" | "getDay" Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(3.0)], Value), "accepts method arguments";
                DateGetHours "getUTCHours" | "getHours" Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(4.0)], Value), "accepts method arguments";
                DateGetMinutes "getUTCMinutes" | "getMinutes" Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(5.0)], Value), "accepts method arguments";
                DateGetSeconds "getUTCSeconds" | "getSeconds" Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(6.0)], Value), "accepts method arguments";
                DateGetMilliseconds "getUTCMilliseconds" | "getMilliseconds" Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(7.0)], Value), "accepts method arguments";
                DateGetTimezoneOffset "getTimezoneOffset" Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(8.0)], Value), "accepts method arguments";
                DateSetFullYear "setUTCFullYear" | "setFullYear" -> [Number] Upstream Method [args:rest] => optional(date_set(0, 3, args)), "accepts date fields";
                DateSetMonth "setUTCMonth" | "setMonth" -> [Number] Upstream Method [args:rest] => optional(date_set(1, 2, args)), "accepts date fields";
                DateSetDate "setUTCDate" | "setDate" -> [Number] Upstream Method [args:rest] => optional(date_set(2, 1, args)), "accepts date fields";
                DateSetHours "setUTCHours" | "setHours" -> [Number] Upstream Method [args:rest] => optional(date_set(3, 4, args)), "accepts date fields";
                DateSetMinutes "setUTCMinutes" | "setMinutes" -> [Number] Upstream Method [args:rest] => optional(date_set(4, 3, args)), "accepts date fields";
                DateSetSeconds "setUTCSeconds" | "setSeconds" -> [Number] Upstream Method [args:rest] => optional(date_set(5, 2, args)), "accepts date fields";
                DateSetMilliseconds "setUTCMilliseconds" | "setMilliseconds" -> [Number] Upstream Method [args:rest] => optional(date_set(6, 1, args)), "accepts date fields";
                DateSetTime "setTime" -> [Number] Upstream Method [args:rest] => optional(date_set(7, 1, args)), "accepts date fields";
            }
            Receiver::Value(Kind::Stats), "Stats" {
                StatsSize "size" Upstream Property [] => intrinsic(StatsSize), "takes no arguments";
                StatsIsFile "isFile" Upstream Method [args:rest] => intrinsic(StatsIsFile, args, [], Value), "accepts method arguments";
                StatsIsDirectory "isDirectory" Upstream Method [args:rest] => intrinsic(StatsIsDirectory, args, [], Value), "accepts method arguments";
            }
            Receiver::Value(Kind::String), "method" {
                StringConcat "concat" -> [String] Upstream Method [args:rest] => value(string_concat(args)), "accepts values";
                StringNormalize "normalize" -> [String] Always Method [form:optional] => optional(string_normalize(form)), "accepts at most one form argument";
                StringLocaleCompare "localeCompare" -> [Number] Upstream Method [other] => value(locale_compare(other)), "requires one string";
                StringSplit "split" -> [StringArray] Always Method [separator] => optional(string_split(separator)), "requires one string or regex separator";
                StringReplace "replace" -> [String] Always Method callback(Replacement) [search, replacement] => optional(replace_string(search, replacement, false)), "requires a search pattern and replacement string";
                StringReplaceAll "replaceAll" -> [String] Always Method callback(Replacement) [search, replacement] => optional(replace_string(search, replacement, true)), "requires a search pattern and replacement string";
                StringMatch "match" -> [NullOptStringArray] Always Method [regex] => optional(string_match(regex)), "requires one literal regex";
                StringSearch "search" -> [Number] Upstream Method [regex] => optional(string_search(regex)), "requires one literal regex";
                StringToString "toString" -> [String] Upstream Method [args:rest] => receiver(args), "accepts method arguments";
            }
            Receiver::Value(Kind::Regex), "method" {
                RegexTest "test" -> [Bool] Upstream Method [text] => value(regex_test_text(text)), "requires one string";
                RegexExec "exec" -> [NullOptStringArray] Always Method [text] => optional(regex_exec(text)), "requires one string";
            }
            Receiver::Value(Kind::Encoder | Kind::Decoder), "TextCodec" {
                CodecEncoding "encoding" -> [String] Upstream Property [] => value(utf8_encoding()), "takes no arguments";
            }
            Receiver::Value(Kind::Encoder), "TextEncoder" {
                EncoderEncode "encode" -> [Bytes] Upstream Method [args:rest] => value(encode(args)), "accepts a string";
            }
            Receiver::Value(Kind::Decoder), "method" {
                DecoderFatal "fatal" Upstream Property [] => intrinsic(DecoderFatal), "takes no arguments";
                DecoderIgnoreBom "ignoreBOM" Upstream Property [] => intrinsic(DecoderIgnoreBom), "takes no arguments";
                DecoderDecode "decode" -> [String] Options Method [bytes:optional, options:optional] => value(decode(bytes, options)), "takes bytes and stream options";
            }
            Receiver::Value(Kind::Url), "method" {
                UrlString "toString" | "toJSON" Upstream Method [args:rest] => intrinsic(UrlGet, args, [Constant(0.0)], Value), "accepts method arguments";
            }
            Receiver::Value(Kind::SearchParams), "method" {
                SearchParamsAppend "append" -> [] Upstream Method [name, value] => value(search_params_append(name, value)), "needs a name and value";
            }
            Receiver::Value(Kind::Abort), "method" {
                AbortSignal "signal" -> Receiver Upstream Property [] => receiver(&[]), "takes no arguments";
                AbortAborted "aborted" Upstream Property [] => intrinsic(AbortAborted), "takes no arguments";
                AbortAbort "abort" -> [] Upstream Method [] => optional(abort_controller()), "does not support abort reasons yet";
            }
            Receiver::Value(Kind::Exception), "Error" {
                ExceptionCause "cause" Upstream Property [] => intrinsic(ExceptionCause), "takes no arguments";
                ExceptionName "name" Upstream Property [] => intrinsic(ExceptionName), "takes no arguments";
                ExceptionMessage "message" -> [String] Upstream Property [] => value(exception_message()), "takes no arguments";
            }
            Receiver::Value(Kind::Number), "method" {
                NumberToString "toString" -> [String] Arguments Method [radix:optional] => optional(number_to_string(radix)), "accepts at most one radix";
            }
            Receiver::Value(Kind::Bytes), "method" {
                BytesSet "set" -> [] Upstream Method [args:rest] => optional(bytes_set(args)), "needs a source and optional offset";
            }
            Receiver::Collection(_) | Receiver::Value(Kind::Array), "method" {
                CollectionKeys "keys" Upstream Method [] => value(iterator_method(IteratorMethod::Keys)), "takes no arguments";
                CollectionValues "values" Upstream Method [] => value(iterator_method(IteratorMethod::Values)), "takes no arguments";
                CollectionEntries "entries" Upstream Method [] => value(iterator_method(IteratorMethod::Entries)), "takes no arguments";
            }
            Receiver::Value(Kind::Instant), "Temporal" {
                InstantEpochMs "epochMilliseconds" Upstream Property [] => intrinsic(InstantMs), "takes no arguments";
                InstantToString "toString" | "toJSON" Upstream Method [] => intrinsic(InstantString), "toString options are not supported yet";
            }
            Receiver::Value(Kind::PlainDate), "Temporal" {
                PlainDateToString "toString" | "toJSON" Upstream Method [] => intrinsic(PlainDateString), "toString options are not supported yet";
            }
            Receiver::Value(Kind::PlainDateTime), "Temporal" {
                PlainDateTimeToString "toString" | "toJSON" Upstream Method [] => intrinsic(PlainDateTimeString), "toString options are not supported yet";
                TemporalHour "hour" -> [Number] Upstream Property [] => value(temporal_part(DatePart::Hour)), "takes no arguments";
                TemporalMinute "minute" -> [Number] Upstream Property [] => value(temporal_part(DatePart::Minute)), "takes no arguments";
                TemporalSecond "second" -> [Number] Upstream Property [] => value(temporal_part(DatePart::Second)), "takes no arguments";
                TemporalMillisecond "millisecond" -> [Number] Upstream Property [] => value(temporal_part(DatePart::Millisecond)), "takes no arguments";
                TemporalMicrosecond "microsecond" -> [Number] Upstream Property [] => value(temporal_part(DatePart::Microsecond)), "takes no arguments";
                TemporalNanosecond "nanosecond" -> [Number] Upstream Property [] => value(temporal_part(DatePart::Nanosecond)), "takes no arguments";
            }
            Receiver::Value(Kind::PlainDate), "Temporal.PlainDate" {
                PlainDateEquals "equals" -> [Bool] Upstream Method [other] => value(plain_date_equals(other)), "takes one argument";
                PlainDateUntil "until" -> Duration Upstream Method [other, options:optional] => value(plain_date_until(other, options)), "takes one or two arguments";
                PlainDateSince "since" -> Duration Upstream Method [other, options:optional] => value(plain_date_since(other, options)), "takes one or two arguments";
            }
            Receiver::Value(Kind::PlainDate | Kind::PlainDateTime), "Temporal" {
                TemporalCalendarId "calendarId" -> [String] Upstream Property [] => value(iso_calendar_id()), "takes no arguments";
                TemporalYear "year" -> [Number] Upstream Property [] => value(temporal_part(DatePart::Year)), "takes no arguments";
                TemporalMonth "month" -> [Number] Upstream Property [] => value(temporal_part(DatePart::Month)), "takes no arguments";
                TemporalDay "day" -> [Number] Upstream Property [] => value(temporal_part(DatePart::Day)), "takes no arguments";
                TemporalDayOfWeek "dayOfWeek" -> [Number] Upstream Property [] => value(temporal_part(DatePart::Weekday)), "takes no arguments";
                TemporalAdd "add" -> Receiver Upstream Method [duration, options:optional] => optional(temporal_add(duration, options, false)), "takes a duration object and optional options";
                TemporalSubtract "subtract" -> Receiver Upstream Method [duration, options:optional] => optional(temporal_add(duration, options, true)), "takes a duration object and optional options";
            }
            Receiver::Value(Kind::Headers), "Headers" {
                HeadersAppend "append" Upstream Method [name, value] => intrinsic(HeadersAppend, [name, value], [Required(Kind::String, "a string"), Required(Kind::String, "a string")], Undefined), "requires 2 string arguments";
                HeadersSet "set" Upstream Method [name, value] => intrinsic(HeadersSet, [name, value], [Required(Kind::String, "a string"), Required(Kind::String, "a string")], Undefined), "requires 2 string arguments";
                HeadersDelete "delete" Upstream Method [name] => intrinsic(HeadersDelete, [name], [Required(Kind::String, "a string")], Undefined), "requires 1 string arguments";
                HeadersGet "get" Upstream Method [name] => intrinsic(HeadersGet, [name], [Required(Kind::String, "a string")], Value), "requires 1 string arguments";
                HeadersHas "has" Upstream Method [name] => intrinsic(HeadersHas, [name], [Required(Kind::String, "a string")], Value), "requires 1 string arguments";
            }
            Receiver::Value(Kind::Response), "Response" {
                ResponseStatus "status" Upstream Property [] => intrinsic(ResponseStatus), "takes no arguments";
                ResponseStatusText "statusText" Upstream Property [] => intrinsic(ResponseStatusText), "takes no arguments";
                ResponseOk "ok" Upstream Property [] => intrinsic(ResponseOk), "takes no arguments";
                ResponseHeaders "headers" Upstream Property [] => intrinsic(ResponseHeaders), "takes no arguments";
                ResponseBody "body" Upstream Property [] => intrinsic(ResponseBody), "takes no arguments";
                ResponseBodyUsed "bodyUsed" Upstream Property [] => intrinsic(ResponseBodyUsed), "takes no arguments";
                ResponseBytes "bytes" Upstream Method [] => intrinsic(ResponseBytes), "takes no arguments";
                ResponseText "text" Upstream Method [] => intrinsic(ResponseText), "takes no arguments";
                ResponseJson "json" Upstream Method [] => intrinsic(ResponseJson), "takes no arguments";
            }
            Receiver::Value(Kind::Request), "Request" {
                RequestVerb "method" Upstream Property [] => intrinsic(RequestMethod), "takes no arguments";
                RequestUrl "url" Upstream Property [] => intrinsic(RequestUrl), "takes no arguments";
                RequestHeaders "headers" Upstream Property [] => intrinsic(RequestHeaders), "takes no arguments";
                RequestBody "body" Upstream Property [] => intrinsic(RequestBody), "takes no arguments";
                RequestBodyUsed "bodyUsed" Upstream Property [] => intrinsic(RequestBodyUsed), "takes no arguments";
                RequestBytes "bytes" Upstream Method [] => intrinsic(RequestBytes), "takes no arguments";
                RequestText "text" Upstream Method [] => intrinsic(RequestText), "takes no arguments";
                RequestJson "json" Upstream Method [] => intrinsic(RequestJson), "takes no arguments";
            }
            Receiver::Value(Kind::Writable), "Writable" {
                WritableLocked "locked" Upstream Property [] => intrinsic(WritableLocked), "takes no arguments";
                WritableGetWriter "getWriter" Upstream Method [] => intrinsic(WritableGetWriter), "takes no arguments";
            }
            Receiver::Value(Kind::Writer), "Writer" {
                WriterWrite "write" -> [PromiseVoid] Upstream Method [chunk] => optional(writer_write(chunk)), "takes one chunk";
                WriterClose "close" Upstream Method [] => intrinsic(WriterClose), "takes no arguments";
                WriterReleaseLock "releaseLock" Upstream Method [] => intrinsic(WriterReleaseLock, [], [], Undefined), "takes no arguments";
            }
            Receiver::Value(Kind::Readable), "Readable" {
                ReadableLocked "locked" Upstream Property [] => intrinsic(ReadableLocked), "takes no arguments";
                ReadableGetReader "getReader" -> StreamReader Upstream Method [options:optional] => optional(readable_get_reader(options)), "takes an optional {mode: 'byob'}";
                ReadableCancel "cancel" Upstream Method [] => intrinsic(ReadableCancel), "takes no arguments";
            }
            Receiver::Value(Kind::Reader), "Reader" {
                ReaderRead "read" Upstream Method [] => intrinsic(ReaderRead), "takes no arguments";
            }
            Receiver::Value(Kind::ByobReader), "ByobReader" {
                ReaderReadByob "read" -> [PromiseReadResult] Upstream Method [view, options:optional] => optional(reader_read_byob(view, options)), "requires a Uint8Array and optional {min}";
            }
            Receiver::Value(Kind::Reader | Kind::ByobReader), "reader" {
                ReaderCancel "cancel" Upstream Method [] => intrinsic(ReaderCancel), "takes no arguments";
                ReaderReleaseLock "releaseLock" Upstream Method [] => intrinsic(ReaderReleaseLock, [], [], Undefined), "takes no arguments";
            }
            Receiver::Value(Kind::ReadResult), "ReadResult" {
                ReadDone "done" Upstream Property [] => intrinsic(ReadDone), "takes no arguments";
                ReadValue "value" Upstream Property [] => intrinsic(ReadValue), "takes no arguments";
            }
        }
    };
}
