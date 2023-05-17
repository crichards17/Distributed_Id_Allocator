require("wasm-tracing-allocator");
import { CompressorFactory, buildHugeCompressor } from "./idCompressorTestUtilities";

describe.only("IdCompressor memory", () => {
	it("Trace allocations", async () => {
		const compressor = buildHugeCompressor();
		dumpAllocations(true);
		dumpAllocations(false);
		console.log(
			"Total WASM heap size: " + require("wasm-id-allocator").__wasm.memory.buffer.byteLength,
		);
		CompressorFactory.disposeCompressor(compressor);
	});
});

function dumpAllocations(showCount: boolean): void {
	(global as any).WasmTracingAllocator.dumpLiveAllocations({
		keyLabel: "Live Allocations",
		valueLabel: showCount ? "Count" : "Bytes",
		getKey: (entry) =>
			entry.stack
				.split(/[\n\s]/)
				.filter(
					(s) =>
						s.indexOf("distributed_id_allocator::") >= 0 ||
						s.indexOf("wasm_id_allocator::") >= 0 ||
						s.indexOf("vec::") >= 0 ||
						s.toLowerCase().indexOf("btree") >= 0,
				)
				.map((str) => {
					return str
						.replace(`distributed_id_allocator::`, "")
						.replace(`wasm_id_allocator::`, "");
				})
				.slice(-5)
				.join("   "),
		getValue: showCount ? (_entry) => 1 : (_entry) => _entry.size,
	});
}
