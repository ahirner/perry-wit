// Conformance Test: Date and Wall Clocks
// Capability: web.date

const d = new Date(1711929600000);
console.log(d.getTime());
console.log(d.toISOString());

const d2 = new Date();
console.log(d2.getTime() > 1700000000000 ? "DATE_NEW_OK" : "DATE_NEW_FAIL");

const now = Date.now();
console.log(now > 1700000000000 ? "DATE_NOW_OK" : "DATE_NOW_FAIL");
