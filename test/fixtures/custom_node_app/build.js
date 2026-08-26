const fs = require("node:fs");

fs.mkdirSync("dist", { recursive: true });
fs.writeFileSync("dist/hello.txt", "hello from boringbuilder\n");
fs.writeFileSync("dist/node.txt", `${process.version}\n`);
