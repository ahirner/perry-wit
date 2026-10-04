// Conformance Test: Date and Wall Clocks
// Capability: web.date

const d = new Date(1711929600000);
if (d.getTime() === 1711929600000) console.log("EPOCH_OK"); else console.log("EPOCH_FAIL");
console.log(d.toISOString());

const d2 = new Date(Date.now());
if (d2.getTime() > 1700000000000) console.log("DATE_NEW_OK"); else console.log("DATE_NEW_FAIL");

const now = Date.now();
if (now > 1700000000000) console.log("DATE_NOW_OK"); else console.log("DATE_NOW_FAIL");
