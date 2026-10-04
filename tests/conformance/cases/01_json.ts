const input = JSON.parse('{"name":"漢🙂","values":[1,2,3],"active":true}');
const output = JSON.stringify(input);
if (typeof output === "string") console.log(output);
else throw 1;
