const path = require("node:path");
const fs = require("fs");

function readStatsOrError(fileName) {
	try {
		return fs.statSync(fileName);
	} catch {
		throw new Error(`File ${fileName} not found`);
	}
}

const size = process.argv[2];
if (size === undefined || Number.isNaN(Number.parseInt(size))) {
	throw new Error("Usage: node wasm-size.js <size>");
}

const stats = readStatsOrError("./dist/wasm/wasm_id_allocator_bg.wasm");
const parsedSize = Number.parseInt(size);
if (stats.size > parsedSize) {
	throw Error(`WASM binary size greater than ${parsedSize} bytes.`);
}
