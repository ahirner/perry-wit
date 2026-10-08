
const r = Math.random();
if (r >= 0.0 && r < 1.0) console.log("MATH_RANDOM_RANGE_OK"); else console.log("MATH_RANDOM_RANGE_FAIL");

const id = crypto.randomUUID();
if (id.length === 36) console.log("UUID_LEN_OK"); else console.log("UUID_LEN_FAIL");
if (id.charAt(8) === '-' && id.charAt(13) === '-' && id.charAt(18) === '-' && id.charAt(23) === '-') console.log("UUID_HYPHENS_OK"); else console.log("UUID_HYPHENS_FAIL");
if (id.charAt(14) === '4') console.log("UUID_V4_OK"); else console.log("UUID_V4_FAIL");
const varChar = id.charAt(19);
if (varChar === '8' || varChar === '9' || varChar === 'a' || varChar === 'b') console.log("UUID_VARIANT_OK"); else console.log("UUID_VARIANT_FAIL");

const bytes = new Uint8Array(8);
const filled = crypto.getRandomValues(bytes);
if (filled === bytes) console.log("GET_RANDOM_VALUES_REF_OK"); else console.log("GET_RANDOM_VALUES_REF_FAIL");
if (bytes.length === 8) console.log("GET_RANDOM_VALUES_LEN_OK"); else console.log("GET_RANDOM_VALUES_LEN_FAIL");
