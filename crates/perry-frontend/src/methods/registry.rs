/// Built-in methods, one row each: variant, source names, inferred result, HIR name preservation,
/// member form, arguments, lowering recipe and arity requirement. `perry-frontend` derives `Method`
/// and `Call` from the signature columns; `perry-lower` derives `Call::lower` from the recipes,
/// which name its `FunctionBuilder` operations.
#[macro_export]
macro_rules! method_catalog {
    ($consumer:ident) => {
        $consumer! {
            Receiver::Value(Kind::Array), "array" {
                ArrayPush "push" Number Push Method [args:rest] => value(push_values(args)), "accepts values";
                ArrayConcat "concat" Receiver Concat Method [args:rest] => value(concat_values(args)), "accepts values";
                ArrayMap "map" Mapped Upstream Method [callback] => value(array_transform(callback, Transform::Map)), "requires one static callback";
                ArrayFilter "filter" Receiver Upstream Method [callback] => value(array_transform(callback, Transform::Filter)), "requires one static callback";
                ArrayFlatMap "flatMap" FlatMapped Upstream Method [callback] => value(array_transform(callback, Transform::FlatMap)), "requires one static callback";
                ArrayFind "find" Element Upstream Method [callback] => value(array_search(Search::Find(callback, FindResult::Value, Direction::Forward))), "requires one static callback";
                ArrayFindIndex "findIndex" Number Upstream Method [callback] => value(array_search(Search::Find(callback, FindResult::Index, Direction::Forward))), "requires one static callback";
                ArrayFindLast "findLast" Element Always Method [callback] => value(array_search(Search::Find(callback, FindResult::Value, Direction::Reverse))), "requires one static callback";
                ArrayFindLastIndex "findLastIndex" Number Always Method [callback] => value(array_search(Search::Find(callback, FindResult::Index, Direction::Reverse))), "requires one static callback";
                ArraySome "some" Boolean Upstream Method [callback] => value(array_search(Search::Quantifier(callback, Quantifier::Some))), "requires one static callback";
                ArrayEvery "every" Boolean Upstream Method [callback] => value(array_search(Search::Quantifier(callback, Quantifier::Every))), "requires one static callback";
                ArrayForEach "forEach" Void Always Method [callback] => value(array_for_each(callback)), "requires one static callback";
                ArraySort "sort" Receiver Upstream Method [comparator:optional] => value(array_sort(comparator, Mutation::InPlace)), "accepts at most one comparator callback";
                ArrayToSorted "toSorted" Receiver Upstream Method [comparator:optional] => value(array_sort(comparator, Mutation::Copy)), "accepts at most one comparator callback";
                ArrayReduce "reduce" Reduced Always Method [callback, initial:optional] => value(array_reduce(callback, initial, Direction::Forward)), "requires a callback and optional initial value";
                ArrayReduceRight "reduceRight" Reduced Always Method [callback, initial:optional] => value(array_reduce(callback, initial, Direction::Reverse)), "requires a callback and optional initial value";
                ArrayJoin "join" String Always Method [separator:optional] => value(array_join(separator)), "accepts at most one separator";
                ArraySlice "slice" Receiver Always Method [start:optional, end:optional] => value(array_slice(start, end)), "accepts at most two bounds";
                ArrayReverse "reverse" Receiver Always Method [] => value(array_reverse(Mutation::InPlace)), "takes no arguments";
                ArrayToReversed "toReversed" Receiver Always Method [] => value(array_reverse(Mutation::Copy)), "takes no arguments";
                ArrayFlat "flat" Flat Always Method [depth:optional] => value(array_flat(depth)), "accepts at most one depth argument";
                ArrayAt "at" At Always Method [index:optional] => value(array_at(index)), "accepts at most one index";
                ArrayPop "pop" Element Always Method [] => value(array_pop()), "takes no arguments";
                ArrayWith "with" Receiver Always Method [index, value] => value(array_with(index, value)), "requires index and value arguments";
                ArrayIncludes "includes" Boolean Upstream Method [value, from:optional] => value(array_search(Search::Includes(value, from))), "requires a value and optional starting index";
                ArrayIndexOf "indexOf" Number Upstream Method [value, from:optional] => value(array_search(Search::Index(value, from, Direction::Forward))), "requires a value and optional starting index";
                ArrayLastIndexOf "lastIndexOf" Number Always Method [value, from:optional] => value(array_search(Search::Index(value, from, Direction::Reverse))), "requires a value and optional starting index";
            }
            Receiver::Map, "Map" {
                MapGet "get" Lookup Upstream Method [key] => value(map_get(key)), "requires one key argument";
                MapSet "set" Receiver Upstream Method [key, value] => value(map_set(key, value)), "requires key and value arguments";
                MapHas "has" Boolean Upstream Method [key] => value(map_has(key)), "requires one key argument";
                MapDelete "delete" Boolean Upstream Method [key] => value(map_delete(key)), "requires one key argument";
                MapClear "clear" Void Upstream Method [] => value(collection_clear()), "takes no arguments";
                MapForEach "forEach" Void Always Method [callback] => value(collection_for_each(callback)), "requires one callback argument";
            }
            Receiver::Set, "Set" {
                SetAdd "add" Receiver Upstream Method [value] => value(set_insert(value)), "requires one value argument";
                SetHas "has" Boolean Upstream Method [value] => value(set_contains(value)), "requires one value argument";
                SetDelete "delete" Boolean Upstream Method [value] => value(set_remove(value)), "requires one value argument";
                SetDifference "difference" Receiver Always Method [other] => value(set_operation(other, SetOperation::Difference)), "requires one Set argument";
                SetIntersection "intersection" Receiver Always Method [other] => value(set_operation(other, SetOperation::Intersection)), "requires one Set argument";
                SetUnion "union" Receiver Always Method [other] => value(set_operation(other, SetOperation::Union)), "requires one Set argument";
                SetSymmetricDifference "symmetricDifference" Receiver Always Method [other] => value(set_operation(other, SetOperation::SymmetricDifference)), "requires one Set argument";
                SetIsSubsetOf "isSubsetOf" Boolean Always Method [other] => value(set_relation(other, SetRelation::Subset)), "requires one Set argument";
                SetIsSupersetOf "isSupersetOf" Boolean Always Method [other] => value(set_relation(other, SetRelation::Superset)), "requires one Set argument";
                SetIsDisjointFrom "isDisjointFrom" Boolean Always Method [other] => value(set_relation(other, SetRelation::Disjoint)), "requires one Set argument";
                SetClear "clear" Void Upstream Method [] => value(collection_clear()), "takes no arguments";
                SetForEach "forEach" Void Always Method [callback] => value(collection_for_each(callback)), "requires one callback argument";
            }
            Receiver::Value(Kind::String), "String" {
                StringCharAt "charAt" String Upstream Method [args:rest] => intrinsic(StringCharAt, args, [Number(0.0)], Value), "accepts method arguments";
                StringCharCodeAt "charCodeAt" Number Upstream Method [args:rest] => intrinsic(StringCharCodeAt, args, [Number(0.0)], Value), "accepts method arguments";
                StringCodePointAt "codePointAt" OptionalNumber Upstream Method [args:rest] => intrinsic(StringCodePointAt, args, [Number(0.0)], Value), "accepts method arguments";
                StringAt "at" At Always Method [args:rest] => intrinsic(StringAt, args, [Number(0.0)], Value), "accepts method arguments";
                StringPadStart "padStart" String Always Method [args:rest] => intrinsic(StringPadStart, args, [Number(0.0), Padding], Value), "accepts method arguments";
                StringPadEnd "padEnd" String Always Method [args:rest] => intrinsic(StringPadEnd, args, [Number(0.0), Padding], Value), "accepts method arguments";
                StringSlice "slice" String Always Method [args:rest] => intrinsic(StringSlice, args, [Number(0.0), Number(f64::INFINITY)], Value), "accepts method arguments";
                StringSubstring "substring" String Upstream Method [args:rest] => intrinsic(StringSubstring, args, [Number(0.0), Number(f64::INFINITY)], Value), "accepts method arguments";
                StringIndexOf "indexOf" Number Upstream Method [args:rest] => intrinsic(StringIndexOf, args, [SearchString, Number(0.0)], Value), "accepts method arguments";
                StringLastIndexOf "lastIndexOf" Number Always Method [args:rest] => intrinsic(StringLastIndexOf, args, [SearchString, Number(f64::INFINITY)], Value), "accepts method arguments";
                StringIncludes "includes" Boolean Upstream Method [args:rest] => intrinsic(StringIndexOf, args, [SearchString, Number(0.0)], Found), "accepts method arguments";
                StringStartsWith "startsWith" Boolean Upstream Method [args:rest] => intrinsic(StringStartsWith, args, [SearchString, Number(0.0)], Value), "accepts method arguments";
                StringEndsWith "endsWith" Boolean Upstream Method [args:rest] => intrinsic(StringEndsWith, args, [SearchString, Number(f64::INFINITY)], Value), "accepts method arguments";
                StringTrim "trim" String Upstream Method [args:rest] => intrinsic(StringTrim, args, [], Value), "accepts method arguments";
                StringTrimStart "trimStart" | "trimLeft" String Always Method [args:rest] => intrinsic(StringTrimStart, args, [], Value), "accepts method arguments";
                StringTrimEnd "trimEnd" | "trimRight" String Always Method [args:rest] => intrinsic(StringTrimEnd, args, [], Value), "accepts method arguments";
                StringToLowerCase "toLowerCase" | "toLocaleLowerCase" String Upstream Method [args:rest] => intrinsic(StringLower, args, [], Value), "accepts method arguments";
                StringToUpperCase "toUpperCase" | "toLocaleUpperCase" String Upstream Method [args:rest] => intrinsic(StringUpper, args, [], Value), "accepts method arguments";
                StringRepeat "repeat" String Upstream Method [args:rest] => intrinsic(StringRepeat, args, [Number(0.0)], Value), "accepts method arguments";
            }
            Receiver::Value(Kind::Bytes), "Bytes" {
                BytesSubarray "subarray" Receiver Upstream Method [args:rest] => intrinsic(BytesSubarray, args, [Number(0.0), Number(f64::INFINITY)], Value), "accepts method arguments";
                BytesSlice "slice" Receiver Always Method [args:rest] => intrinsic(BytesSlice, args, [Number(0.0), Number(f64::INFINITY)], Value), "accepts method arguments";
                BytesFill "fill" Receiver Upstream Method [args:rest] => intrinsic(BytesFill, args, [Number(0.0), Number(0.0), Number(f64::INFINITY)], Receiver), "accepts method arguments";
                BytesCryptoFillRandom "$$cryptoFillRandom" Receiver Upstream Method [args:rest] => intrinsic(BytesFillRandom, args, [], Value), "accepts method arguments";
            }
            Receiver::Value(Kind::Date), "Date" {
                DateGetTime "getTime" | "valueOf" Number Upstream Method [args:rest] => intrinsic(DateTime, args, [], Value), "accepts method arguments";
                DateToISOString "toISOString" String Upstream Method [args:rest] => intrinsic(DateIso, args, [], Value), "accepts method arguments";
                DateToJSON "toJSON" NullableString Upstream Method [args:rest] => intrinsic(DateJson, args, [], Value), "accepts method arguments";
                DateGetFullYear "getUTCFullYear" | "getFullYear" Number Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(0.0)], Value), "accepts method arguments";
                DateGetMonth "getUTCMonth" | "getMonth" Number Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(1.0)], Value), "accepts method arguments";
                DateGetDate "getUTCDate" | "getDate" Number Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(2.0)], Value), "accepts method arguments";
                DateGetDay "getUTCDay" | "getDay" Number Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(3.0)], Value), "accepts method arguments";
                DateGetHours "getUTCHours" | "getHours" Number Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(4.0)], Value), "accepts method arguments";
                DateGetMinutes "getUTCMinutes" | "getMinutes" Number Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(5.0)], Value), "accepts method arguments";
                DateGetSeconds "getUTCSeconds" | "getSeconds" Number Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(6.0)], Value), "accepts method arguments";
                DateGetMilliseconds "getUTCMilliseconds" | "getMilliseconds" Number Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(7.0)], Value), "accepts method arguments";
                DateGetTimezoneOffset "getTimezoneOffset" Number Upstream Method [_args:rest] => intrinsic(DatePart, [], [Constant(8.0)], Value), "accepts method arguments";
                DateSetFullYear "setUTCFullYear" | "setFullYear" Number Upstream Method [args:rest] => optional(date_set(0, 3, args)), "accepts date fields";
                DateSetMonth "setUTCMonth" | "setMonth" Number Upstream Method [args:rest] => optional(date_set(1, 2, args)), "accepts date fields";
                DateSetDate "setUTCDate" | "setDate" Number Upstream Method [args:rest] => optional(date_set(2, 1, args)), "accepts date fields";
                DateSetHours "setUTCHours" | "setHours" Number Upstream Method [args:rest] => optional(date_set(3, 4, args)), "accepts date fields";
                DateSetMinutes "setUTCMinutes" | "setMinutes" Number Upstream Method [args:rest] => optional(date_set(4, 3, args)), "accepts date fields";
                DateSetSeconds "setUTCSeconds" | "setSeconds" Number Upstream Method [args:rest] => optional(date_set(5, 2, args)), "accepts date fields";
                DateSetMilliseconds "setUTCMilliseconds" | "setMilliseconds" Number Upstream Method [args:rest] => optional(date_set(6, 1, args)), "accepts date fields";
                DateSetTime "setTime" Number Upstream Method [args:rest] => optional(date_set(7, 1, args)), "accepts date fields";
            }
            Receiver::Value(Kind::Stats), "Stats" {
                StatsIsFile "isFile" Boolean Upstream Method [args:rest] => intrinsic(StatsIsFile, args, [], Value), "accepts method arguments";
                StatsIsDirectory "isDirectory" Boolean Upstream Method [args:rest] => intrinsic(StatsIsDirectory, args, [], Value), "accepts method arguments";
            }
            Receiver::Value(Kind::String), "method" {
                StringConcat "concat" String Upstream Method [args:rest] => value(string_concat(args)), "accepts values";
                StringNormalize "normalize" String Always Method [form:optional] => optional(string_normalize(form)), "accepts at most one form argument";
                StringLocaleCompare "localeCompare" Number Upstream Method [other] => value(locale_compare(other)), "requires one string";
                StringSplit "split" Strings Always Method [separator] => optional(string_split(separator)), "requires one string or regex separator";
                StringReplace "replace" String Always Method [search, replacement] => optional(replace_string(search, replacement, false)), "requires a search pattern and replacement string";
                StringReplaceAll "replaceAll" String Always Method [search, replacement] => optional(replace_string(search, replacement, true)), "requires a search pattern and replacement string";
                StringMatch "match" NullableStrings Always Method [regex] => optional(string_match(regex)), "requires one literal regex";
                StringSearch "search" Number Upstream Method [regex] => optional(string_search(regex)), "requires one literal regex";
                StringToString "toString" String Upstream Method [args:rest] => receiver(args), "accepts method arguments";
            }
            Receiver::Value(Kind::Regex), "method" {
                RegexTest "test" Boolean Upstream Method [text] => value(regex_test_text(text)), "requires one string";
                RegexExec "exec" NullableStrings Always Method [text] => optional(regex_exec(text)), "requires one string";
            }
            Receiver::Value(Kind::Decoder), "method" {
                DecoderDecode "decode" String Upstream Method [bytes:optional, options:optional] => value(decode(bytes, options)), "takes bytes and stream options";
            }
            Receiver::Value(Kind::Url), "method" {
                UrlString "toString" | "toJSON" String Upstream Method [args:rest] => intrinsic(UrlGet, args, [Constant(0.0)], Value), "accepts method arguments";
            }
            Receiver::Value(Kind::SearchParams), "method" {
                SearchParamsAppend "append" Void Upstream Method [name, value] => value(search_params_append(name, value)), "needs a name and value";
            }
            Receiver::Value(Kind::Abort), "method" {
                AbortAbort "abort" Void Upstream Method [] => optional(abort_controller()), "does not support abort reasons yet";
            }
            Receiver::Value(Kind::Number), "method" {
                NumberToString "toString" String Arguments Method [radix:optional] => optional(number_to_string(radix)), "accepts at most one radix";
            }
            Receiver::Value(Kind::Bytes), "method" {
                BytesSet "set" Void Upstream Method [args:rest] => optional(bytes_set(args)), "needs a source and optional offset";
            }
            Receiver::Map | Receiver::Set | Receiver::Value(Kind::Array), "method" {
                CollectionKeys "keys" IteratorKeys Upstream Method [] => value(iterator_method(IteratorMethod::Keys)), "takes no arguments";
                CollectionValues "values" IteratorValues Upstream Method [] => value(iterator_method(IteratorMethod::Values)), "takes no arguments";
                CollectionEntries "entries" IteratorEntries Upstream Method [] => value(iterator_method(IteratorMethod::Entries)), "takes no arguments";
            }
            Receiver::Value(Kind::Instant), "Temporal" {
                InstantToString "toString" | "toJSON" String Upstream Method [] => intrinsic(InstantString), "toString options are not supported yet";
            }
            Receiver::Value(Kind::PlainDate), "Temporal" {
                PlainDateToString "toString" | "toJSON" String Upstream Method [] => intrinsic(PlainDateString), "toString options are not supported yet";
            }
            Receiver::Value(Kind::PlainDateTime), "Temporal" {
                PlainDateTimeToString "toString" | "toJSON" String Upstream Method [] => intrinsic(PlainDateTimeString), "toString options are not supported yet";
            }
            Receiver::Value(Kind::PlainDate), "Temporal.PlainDate" {
                PlainDateEquals "equals" Boolean Upstream Method [other] => value(plain_date_equals(other)), "takes one argument";
                PlainDateUntil "until" Duration Upstream Method [other, options:optional] => value(plain_date_until(other, options)), "takes one or two arguments";
                PlainDateSince "since" Duration Upstream Method [other, options:optional] => value(plain_date_since(other, options)), "takes one or two arguments";
            }
            Receiver::Value(Kind::PlainDate | Kind::PlainDateTime), "Temporal" {
                TemporalAdd "add" Receiver Upstream Method [duration, options:optional] => optional(temporal_add(duration, options, false)), "takes a duration object and optional options";
                TemporalSubtract "subtract" Receiver Upstream Method [duration, options:optional] => optional(temporal_add(duration, options, true)), "takes a duration object and optional options";
            }
            Receiver::Value(Kind::Headers), "Headers" {
                HeadersAppend "append" Void Upstream Method [name, value] => intrinsic(HeadersAppend, [name, value], [Required(Kind::String, "a string"), Required(Kind::String, "a string")], Undefined), "requires 2 string arguments";
                HeadersSet "set" Void Upstream Method [name, value] => intrinsic(HeadersSet, [name, value], [Required(Kind::String, "a string"), Required(Kind::String, "a string")], Undefined), "requires 2 string arguments";
                HeadersDelete "delete" Void Upstream Method [name] => intrinsic(HeadersDelete, [name], [Required(Kind::String, "a string")], Undefined), "requires 1 string arguments";
                HeadersGet "get" NullableString Upstream Method [name] => intrinsic(HeadersGet, [name], [Required(Kind::String, "a string")], Value), "requires 1 string arguments";
                HeadersHas "has" Boolean Upstream Method [name] => intrinsic(HeadersHas, [name], [Required(Kind::String, "a string")], Value), "requires 1 string arguments";
            }
            Receiver::Value(Kind::Response), "Response" {
                ResponseStatus "status" Number Upstream Property [] => intrinsic(ResponseStatus), "takes no arguments";
                ResponseStatusText "statusText" String Upstream Property [] => intrinsic(ResponseStatusText), "takes no arguments";
                ResponseOk "ok" Boolean Upstream Property [] => intrinsic(ResponseOk), "takes no arguments";
                ResponseHeaders "headers" Native(Kind::Headers) Upstream Property [] => intrinsic(ResponseHeaders), "takes no arguments";
                ResponseBody "body" Nullable(Kind::Readable) Upstream Property [] => intrinsic(ResponseBody), "takes no arguments";
                ResponseBodyUsed "bodyUsed" Boolean Upstream Property [] => intrinsic(ResponseBodyUsed), "takes no arguments";
                ResponseBytes "bytes" Promise(Kind::Bytes) Upstream Method [] => intrinsic(ResponseBytes), "takes no arguments";
                ResponseText "text" Promise(Kind::String) Upstream Method [] => intrinsic(ResponseText), "takes no arguments";
                ResponseJson "json" PromiseJson Upstream Method [] => intrinsic(ResponseJson), "takes no arguments";
            }
            Receiver::Value(Kind::Request), "Request" {
                RequestVerb "method" String Upstream Property [] => intrinsic(RequestMethod), "takes no arguments";
                RequestUrl "url" String Upstream Property [] => intrinsic(RequestUrl), "takes no arguments";
                RequestHeaders "headers" Native(Kind::Headers) Upstream Property [] => intrinsic(RequestHeaders), "takes no arguments";
                RequestBody "body" Nullable(Kind::Readable) Upstream Property [] => intrinsic(RequestBody), "takes no arguments";
                RequestBodyUsed "bodyUsed" Boolean Upstream Property [] => intrinsic(RequestBodyUsed), "takes no arguments";
                RequestBytes "bytes" Promise(Kind::Bytes) Upstream Method [] => intrinsic(RequestBytes), "takes no arguments";
                RequestText "text" Promise(Kind::String) Upstream Method [] => intrinsic(RequestText), "takes no arguments";
                RequestJson "json" PromiseJson Upstream Method [] => intrinsic(RequestJson), "takes no arguments";
            }
            Receiver::Value(Kind::Writable), "Writable" {
                WritableLocked "locked" Boolean Upstream Property [] => intrinsic(WritableLocked), "takes no arguments";
                WritableGetWriter "getWriter" Native(Kind::Writer) Upstream Method [] => intrinsic(WritableGetWriter), "takes no arguments";
            }
            Receiver::Value(Kind::Writer), "Writer" {
                WriterWrite "write" PromiseVoid Upstream Method [chunk] => optional(writer_write(chunk)), "takes one chunk";
                WriterClose "close" PromiseVoid Upstream Method [] => intrinsic(WriterClose), "takes no arguments";
                WriterReleaseLock "releaseLock" Void Upstream Method [] => intrinsic(WriterReleaseLock, [], [], Undefined), "takes no arguments";
            }
            Receiver::Value(Kind::Readable), "Readable" {
                ReadableLocked "locked" Boolean Upstream Property [] => intrinsic(ReadableLocked), "takes no arguments";
                ReadableGetReader "getReader" StreamReader Upstream Method [options:optional] => optional(readable_get_reader(options)), "takes an optional {mode: 'byob'}";
                ReadableCancel "cancel" PromiseVoid Upstream Method [] => intrinsic(ReadableCancel), "takes no arguments";
            }
            Receiver::Value(Kind::Reader), "Reader" {
                ReaderRead "read" Promise(Kind::ReadResult) Upstream Method [] => intrinsic(ReaderRead), "takes no arguments";
            }
            Receiver::Value(Kind::ByobReader), "ByobReader" {
                ReaderReadByob "read" Promise(Kind::ReadResult) Upstream Method [view, options:optional] => optional(reader_read_byob(view, options)), "requires a Uint8Array and optional {min}";
            }
            Receiver::Value(Kind::Reader | Kind::ByobReader), "reader" {
                ReaderCancel "cancel" PromiseVoid Upstream Method [] => intrinsic(ReaderCancel), "takes no arguments";
                ReaderReleaseLock "releaseLock" Void Upstream Method [] => intrinsic(ReaderReleaseLock, [], [], Undefined), "takes no arguments";
            }
            Receiver::Value(Kind::ReadResult), "ReadResult" {
                ReadDone "done" Boolean Upstream Property [] => intrinsic(ReadDone), "takes no arguments";
                ReadValue "value" Optional(Kind::Bytes) Upstream Property [] => intrinsic(ReadValue), "takes no arguments";
            }
        }
    };
}
