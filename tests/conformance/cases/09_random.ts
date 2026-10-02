// Conformance Test: Randomness and Cryptography
// Capability: web.random

const r = Math.random();
console.log(r >= 0.0 && r < 1.0 ? "MATH_RANDOM_RANGE_OK" : "MATH_RANDOM_RANGE_FAIL");

const id = crypto.randomUUID();
console.log(id.length === 36 ? "UUID_LEN_OK" : "UUID_LEN_FAIL");
console.log(id.charAt(8) === '-' && id.charAt(13) === '-' && id.charAt(18) === '-' && id.charAt(23) === '-' ? "UUID_HYPHENS_OK" : "UUID_HYPHENS_FAIL");
console.log(id.charAt(14) === '4' ? "UUID_V4_OK" : "UUID_V4_FAIL");
const varChar = id.charAt(19);
console.log(varChar === '8' || varChar === '9' || varChar === 'a' || varChar === 'b' ? "UUID_VARIANT_OK" : "UUID_VARIANT_FAIL");

const bytes = new Uint8Array(8);
const filled = crypto.getRandomValues(bytes);
console.log(filled === bytes ? "GET_RANDOM_VALUES_REF_OK" : "GET_RANDOM_VALUES_REF_FAIL");
console.log(bytes.length === 8 ? "GET_RANDOM_VALUES_LEN_OK" : "GET_RANDOM_VALUES_LEN_FAIL");
