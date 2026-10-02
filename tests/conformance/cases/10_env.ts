// Conformance Test: Process Environment, Arguments, and CWD
// Capability: cli.env

process.env.TEST_KEY = "conformance_test_val";
console.log("MUTATED=" + (process.env.TEST_KEY === "conformance_test_val"));

process.env.ANOTHER = "42";
console.log("ANOTHER=" + process.env.ANOTHER);

console.log("ARGV_IS_ARRAY=" + Array.isArray(process.argv));
console.log("ARGV_HAS_LEN=" + (process.argv.length >= 1));

const cwd = process.cwd();
console.log("CWD_STRING=" + (typeof cwd === "string" && cwd.length > 0));

const keys = Object.keys(process.env);
console.log("KEYS_IS_ARRAY=" + Array.isArray(keys));
console.log("KEYS_HAS_TEST_KEY=" + keys.includes("TEST_KEY"));

const json = JSON.stringify(process.env);
console.log("JSON_HAS_TEST_KEY=" + json.includes('"TEST_KEY":"conformance_test_val"'));
