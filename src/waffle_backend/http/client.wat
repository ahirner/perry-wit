  (import "wasi:http/client@0.3.0" (instance $http-client
    (alias outer 1 $http-request (type $request-base))
    (alias outer 1 $http-response (type $response-base))
    (alias outer 1 $http-error (type $error-base))
    (export "request" (type $request (eq $request-base)))
    (export "response" (type $response (eq $response-base)))
    (export "error-code" (type $error (eq $error-base)))
    (export "send" (func async (param "request" (own $request)) (result (result (own $response) (error $error)))))))
