if (process.env.PERRY_CONFORMANCE === 'fixture') console.log('ENV_OK'); else console.log('ENV_FAIL');
if (process.env.PERRY_MISSING_FIXTURE === undefined) console.log('MISSING_OK'); else console.log('MISSING_FAIL');
const keys: string[] = Object.keys(process.env);
let found = false;
for (let i = 0; i < keys.length; i++) {
  if (keys[i] === 'PERRY_CONFORMANCE') found = true;
}
if (found) console.log('KEYS_OK'); else console.log('KEYS_FAIL');
