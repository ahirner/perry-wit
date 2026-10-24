registry! {
    Receiver::Value(Kind::Array), "array" {
        ArrayPush "push" Number Push Method [args:rest], "accepts values";
        ArrayConcat "concat" Receiver Concat Method [args:rest], "accepts values";
        ArrayMap "map" Mapped Upstream Method [callback], "requires one static callback";
        ArrayFilter "filter" Receiver Upstream Method [callback], "requires one static callback";
        ArrayFlatMap "flatMap" FlatMapped Upstream Method [callback], "requires one static callback";
        ArrayFind "find" Element Upstream Method [callback], "requires one static callback";
        ArrayFindIndex "findIndex" Number Upstream Method [callback], "requires one static callback";
        ArrayFindLast "findLast" Element Always Method [callback], "requires one static callback";
        ArrayFindLastIndex "findLastIndex" Number Always Method [callback], "requires one static callback";
        ArraySome "some" Boolean Upstream Method [callback], "requires one static callback";
        ArrayEvery "every" Boolean Upstream Method [callback], "requires one static callback";
        ArrayForEach "forEach" Void Always Method [callback], "requires one static callback";
        ArraySort "sort" Receiver Upstream Method [comparator:optional], "accepts at most one comparator callback";
        ArrayToSorted "toSorted" Receiver Upstream Method [comparator:optional], "accepts at most one comparator callback";
        ArrayReduce "reduce" Reduced Always Method [callback, initial:optional], "requires a callback and optional initial value";
        ArrayReduceRight "reduceRight" Reduced Always Method [callback, initial:optional], "requires a callback and optional initial value";
        ArrayJoin "join" String Always Method [separator:optional], "accepts at most one separator";
        ArraySlice "slice" Receiver Always Method [start:optional, end:optional], "accepts at most two bounds";
        ArrayReverse "reverse" Receiver Always Method [], "takes no arguments";
        ArrayToReversed "toReversed" Receiver Always Method [], "takes no arguments";
        ArrayFlat "flat" Flat Always Method [depth:optional], "accepts at most one depth argument";
        ArrayAt "at" At Always Method [index:optional], "accepts at most one index";
        ArrayPop "pop" Element Always Method [], "takes no arguments";
        ArrayWith "with" Receiver Always Method [index, value], "requires index and value arguments";
        ArrayIncludes "includes" Boolean Upstream Method [value, from:optional], "requires a value and optional starting index";
        ArrayIndexOf "indexOf" Number Upstream Method [value, from:optional], "requires a value and optional starting index";
        ArrayLastIndexOf "lastIndexOf" Number Always Method [value, from:optional], "requires a value and optional starting index";
    }
    Receiver::Map, "Map" {
        MapGet "get" Lookup Upstream Method [key], "requires one key argument";
        MapSet "set" Receiver Upstream Method [key, value], "requires key and value arguments";
        MapHas "has" Boolean Upstream Method [key], "requires one key argument";
        MapDelete "delete" Boolean Upstream Method [key], "requires one key argument";
    }
    Receiver::Map, "Map" {
        MapClear "clear" Void Upstream Method [], "takes no arguments";
        MapForEach "forEach" Void Always Method [callback], "requires one callback argument";
    }
    Receiver::Set, "Set" {
        SetAdd "add" Receiver Upstream Method [value], "requires one value argument";
        SetHas "has" Boolean Upstream Method [value], "requires one value argument";
        SetDelete "delete" Boolean Upstream Method [value], "requires one value argument";
        SetDifference "difference" Receiver Always Method [other], "requires one Set argument";
        SetIntersection "intersection" Receiver Always Method [other], "requires one Set argument";
        SetUnion "union" Receiver Always Method [other], "requires one Set argument";
        SetSymmetricDifference "symmetricDifference" Receiver Always Method [other], "requires one Set argument";
        SetIsSubsetOf "isSubsetOf" Boolean Always Method [other], "requires one Set argument";
        SetIsSupersetOf "isSupersetOf" Boolean Always Method [other], "requires one Set argument";
        SetIsDisjointFrom "isDisjointFrom" Boolean Always Method [other], "requires one Set argument";
    }
    Receiver::Set, "Set" {
        SetClear "clear" Void Upstream Method [], "takes no arguments";
        SetForEach "forEach" Void Always Method [callback], "requires one callback argument";
    }
    Receiver::Value(Kind::String), "String" {
        StringCharAt "charAt" String Upstream Method [args:rest], "accepts method arguments";
        StringCodePointAt "codePointAt" OptionalNumber Upstream Method [args:rest], "accepts method arguments";
        StringAt "at" At Always Method [args:rest], "accepts method arguments";
        StringPadStart "padStart" String Always Method [args:rest], "accepts method arguments";
        StringPadEnd "padEnd" String Always Method [args:rest], "accepts method arguments";
        StringSlice "slice" String Always Method [args:rest], "accepts method arguments";
        StringSubstring "substring" String Upstream Method [args:rest], "accepts method arguments";
        StringIndexOf "indexOf" Number Upstream Method [args:rest], "accepts method arguments";
        StringLastIndexOf "lastIndexOf" Number Always Method [args:rest], "accepts method arguments";
        StringIncludes "includes" Boolean Upstream Method [args:rest], "accepts method arguments";
        StringStartsWith "startsWith" Boolean Upstream Method [args:rest], "accepts method arguments";
        StringEndsWith "endsWith" Boolean Upstream Method [args:rest], "accepts method arguments";
        StringTrim "trim" String Upstream Method [args:rest], "accepts method arguments";
        StringTrimStart "trimStart" | "trimLeft" String Always Method [args:rest], "accepts method arguments";
        StringTrimEnd "trimEnd" | "trimRight" String Always Method [args:rest], "accepts method arguments";
        StringToLowerCase "toLowerCase" | "toLocaleLowerCase" String Upstream Method [args:rest], "accepts method arguments";
        StringToUpperCase "toUpperCase" | "toLocaleUpperCase" String Upstream Method [args:rest], "accepts method arguments";
        StringRepeat "repeat" String Upstream Method [args:rest], "accepts method arguments";
    }
    Receiver::Value(Kind::Bytes), "Bytes" {
        BytesSubarray "subarray" Receiver Upstream Method [args:rest], "accepts method arguments";
        BytesSlice "slice" Receiver Always Method [args:rest], "accepts method arguments";
        BytesFill "fill" Receiver Upstream Method [args:rest], "accepts method arguments";
        BytesCryptoFillRandom "$$cryptoFillRandom" Receiver Upstream Method [args:rest], "accepts method arguments";
    }
    Receiver::Value(Kind::Date), "Date" {
        DateGetTime "getTime" | "valueOf" Number Upstream Method [args:rest], "accepts method arguments";
        DateToISOString "toISOString" String Upstream Method [args:rest], "accepts method arguments";
        DateToJSON "toJSON" NullableString Upstream Method [args:rest], "accepts method arguments";
        DateGetFullYear "getUTCFullYear" | "getFullYear" Number Upstream Method [_args:rest], "accepts method arguments";
        DateGetMonth "getUTCMonth" | "getMonth" Number Upstream Method [_args:rest], "accepts method arguments";
        DateGetDate "getUTCDate" | "getDate" Number Upstream Method [_args:rest], "accepts method arguments";
        DateGetDay "getUTCDay" | "getDay" Number Upstream Method [_args:rest], "accepts method arguments";
        DateGetHours "getUTCHours" | "getHours" Number Upstream Method [_args:rest], "accepts method arguments";
        DateGetMinutes "getUTCMinutes" | "getMinutes" Number Upstream Method [_args:rest], "accepts method arguments";
        DateGetSeconds "getUTCSeconds" | "getSeconds" Number Upstream Method [_args:rest], "accepts method arguments";
        DateGetMilliseconds "getUTCMilliseconds" | "getMilliseconds" Number Upstream Method [_args:rest], "accepts method arguments";
        DateGetTimezoneOffset "getTimezoneOffset" Number Upstream Method [_args:rest], "accepts method arguments";
        DateSetFullYear "setUTCFullYear" | "setFullYear" Number Upstream Method [args:rest], "accepts date fields";
        DateSetMonth "setUTCMonth" | "setMonth" Number Upstream Method [args:rest], "accepts date fields";
        DateSetDate "setUTCDate" | "setDate" Number Upstream Method [args:rest], "accepts date fields";
        DateSetHours "setUTCHours" | "setHours" Number Upstream Method [args:rest], "accepts date fields";
        DateSetMinutes "setUTCMinutes" | "setMinutes" Number Upstream Method [args:rest], "accepts date fields";
        DateSetSeconds "setUTCSeconds" | "setSeconds" Number Upstream Method [args:rest], "accepts date fields";
        DateSetMilliseconds "setUTCMilliseconds" | "setMilliseconds" Number Upstream Method [args:rest], "accepts date fields";
        DateSetTime "setTime" Number Upstream Method [args:rest], "accepts date fields";
    }
    Receiver::Value(Kind::Stats), "Stats" {
        StatsIsFile "isFile" Boolean Upstream Method [args:rest], "accepts method arguments";
        StatsIsDirectory "isDirectory" Boolean Upstream Method [args:rest], "accepts method arguments";
    }
    Receiver::Value(Kind::String), "method" {
        StringConcat "concat" String Upstream Method [args:rest], "accepts values";
        StringNormalize "normalize" String Always Method [form:optional], "accepts at most one form argument";
        StringLocaleCompare "localeCompare" Number Upstream Method [other], "requires one string";
        StringSplit "split" Strings Always Method [separator], "requires one string or regex separator";
        StringReplace "replace" String Always Method [search, replacement], "requires a search pattern and replacement string";
        StringReplaceAll "replaceAll" String Always Method [search, replacement], "requires a search pattern and replacement string";
        StringMatch "match" NullableStrings Always Method [regex], "requires one literal regex";
        StringSearch "search" Number Upstream Method [regex], "requires one literal regex";
        StringCharCodeAt "charCodeAt" Number Upstream Method [_args:rest], "accepts method arguments";
        StringToString "toString" String Upstream Method [_args:rest], "accepts method arguments";
    }
    Receiver::Value(Kind::Regex), "method" {
        RegexTest "test" Boolean Upstream Method [text], "requires one string";
        RegexExec "exec" NullableStrings Always Method [text], "requires one string";
    }
    Receiver::Value(Kind::Decoder), "method" {
        DecoderDecode "decode" String Upstream Method [bytes:optional, options:optional], "takes bytes and stream options";
    }
    Receiver::Value(Kind::Url), "method" {
        UrlString "toString" | "toJSON" String Upstream Method [_args:rest], "accepts method arguments";
    }
    Receiver::Value(Kind::SearchParams), "method" {
        SearchParamsAppend "append" Void Upstream Method [name, value], "needs a name and value";
    }
    Receiver::Value(Kind::Abort), "method" {
        AbortAbort "abort" Void Upstream Method [], "does not support abort reasons yet";
    }
    Receiver::Value(Kind::Number), "method" {
        NumberToString "toString" String Arguments Method [radix:optional], "accepts at most one radix";
    }
    Receiver::Value(Kind::Bytes), "method" {
        BytesSet "set" Void Upstream Method [args:rest], "needs a source and optional offset";
    }
    Receiver::Map | Receiver::Set | Receiver::Value(Kind::Array), "method" {
        CollectionKeys "keys" IteratorKeys Upstream Method [], "takes no arguments";
        CollectionValues "values" IteratorValues Upstream Method [], "takes no arguments";
        CollectionEntries "entries" IteratorEntries Upstream Method [], "takes no arguments";
    }
    Receiver::Value(Kind::Instant), "Temporal" {
        InstantToString "toString" | "toJSON" String Upstream Method [], "toString options are not supported yet";
    }
    Receiver::Value(Kind::PlainDate), "Temporal" {
        PlainDateToString "toString" | "toJSON" String Upstream Method [], "toString options are not supported yet";
    }
    Receiver::Value(Kind::PlainDateTime), "Temporal" {
        PlainDateTimeToString "toString" | "toJSON" String Upstream Method [], "toString options are not supported yet";
    }
    Receiver::Value(Kind::PlainDate), "Temporal.PlainDate" {
        PlainDateEquals "equals" Boolean Upstream Method [other], "takes one argument";
        PlainDateUntil "until" Duration Upstream Method [other, options:optional], "takes one or two arguments";
        PlainDateSince "since" Duration Upstream Method [other, options:optional], "takes one or two arguments";
    }
    Receiver::Value(Kind::PlainDate | Kind::PlainDateTime), "Temporal" {
        TemporalAdd "add" Receiver Upstream Method [duration, options:optional], "takes a duration object and optional options";
        TemporalSubtract "subtract" Receiver Upstream Method [duration, options:optional], "takes a duration object and optional options";
    }
    Receiver::Value(Kind::Headers), "Headers" {
        HeadersAppend "append" Void Upstream Method [name, value], "requires 2 string arguments";
        HeadersSet "set" Void Upstream Method [name, value], "requires 2 string arguments";
        HeadersDelete "delete" Void Upstream Method [name], "requires 1 string arguments";
        HeadersGet "get" NullableString Upstream Method [name], "requires 1 string arguments";
        HeadersHas "has" Boolean Upstream Method [name], "requires 1 string arguments";
    }
    Receiver::Value(Kind::Response), "Response" {
        ResponseStatus "status" Number Upstream Property [], "takes no arguments";
        ResponseStatusText "statusText" String Upstream Property [], "takes no arguments";
        ResponseOk "ok" Boolean Upstream Property [], "takes no arguments";
        ResponseHeaders "headers" Native(Kind::Headers) Upstream Property [], "takes no arguments";
        ResponseBody "body" Nullable(Kind::Readable) Upstream Property [], "takes no arguments";
        ResponseBodyUsed "bodyUsed" Boolean Upstream Property [], "takes no arguments";
        ResponseBytes "bytes" Promise(Kind::Bytes) Upstream Method [], "takes no arguments";
        ResponseText "text" Promise(Kind::String) Upstream Method [], "takes no arguments";
        ResponseJson "json" PromiseJson Upstream Method [], "takes no arguments";
    }
    Receiver::Value(Kind::Request), "Request" {
        RequestVerb "method" String Upstream Property [], "takes no arguments";
        RequestUrl "url" String Upstream Property [], "takes no arguments";
        RequestHeaders "headers" Native(Kind::Headers) Upstream Property [], "takes no arguments";
        RequestBody "body" Nullable(Kind::Readable) Upstream Property [], "takes no arguments";
        RequestBodyUsed "bodyUsed" Boolean Upstream Property [], "takes no arguments";
        RequestBytes "bytes" Promise(Kind::Bytes) Upstream Method [], "takes no arguments";
        RequestText "text" Promise(Kind::String) Upstream Method [], "takes no arguments";
        RequestJson "json" PromiseJson Upstream Method [], "takes no arguments";
    }
    Receiver::Value(Kind::Writable), "Writable" {
        WritableLocked "locked" Boolean Upstream Property [], "takes no arguments";
        WritableGetWriter "getWriter" Native(Kind::Writer) Upstream Method [], "takes no arguments";
    }
    Receiver::Value(Kind::Writer), "Writer" {
        WriterWrite "write" PromiseVoid Upstream Method [chunk], "takes one chunk";
        WriterClose "close" PromiseVoid Upstream Method [], "takes no arguments";
        WriterReleaseLock "releaseLock" Void Upstream Method [], "takes no arguments";
    }
    Receiver::Value(Kind::Readable), "Readable" {
        ReadableLocked "locked" Boolean Upstream Property [], "takes no arguments";
        ReadableGetReader "getReader" StreamReader Upstream Method [options:optional], "takes an optional {mode: 'byob'}";
        ReadableCancel "cancel" PromiseVoid Upstream Method [], "takes no arguments";
    }
    Receiver::Value(Kind::Reader), "Reader" {
        ReaderRead "read" Promise(Kind::ReadResult) Upstream Method [], "takes no arguments";
    }
    Receiver::Value(Kind::ByobReader), "ByobReader" {
        ReaderReadByob "read" Promise(Kind::ReadResult) Upstream Method [view, options:optional], "requires a Uint8Array and optional {min}";
    }
    Receiver::Value(Kind::Reader | Kind::ByobReader), "reader" {
        ReaderCancel "cancel" PromiseVoid Upstream Method [], "takes no arguments";
        ReaderReleaseLock "releaseLock" Void Upstream Method [], "takes no arguments";
    }
    Receiver::Value(Kind::ReadResult), "ReadResult" {
        ReadDone "done" Boolean Upstream Property [], "takes no arguments";
        ReadValue "value" Optional(Kind::Bytes) Upstream Property [], "takes no arguments";
    }
}
