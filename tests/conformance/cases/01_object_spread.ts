// Conformance Test: Object Spread Operator ({ ...a, ...b })
// Capability: web.object_spread

const a = JSON.parse('{"id":"item-1","title":"Original Title","version":1}');
const b = JSON.parse('{"title":"Updated Title","version":2,"extra":"bonus"}');

const merged = { ...a, ...b };
console.log(JSON.stringify(merged));
