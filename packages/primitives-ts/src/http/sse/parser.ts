import { encodeUtf8 } from "../../encoding/utf8/index.ts";
import {
	createErrorResult,
	createSuccessResult,
	type Result,
} from "../../results/index.ts";
import type { SseEvent } from "./event.ts";

type LinePosition = "initial" | "subsequent" | "after-cr";
type FrameBuffer = {
	/** The input bytes of this frame's `data` lines, including line breaks. */
	dataSize: number;
	/** The input bytes of this frame's last `event` line, including its line break. */
	eventTypeSize: number;
	/** The input bytes of this frame's last accepted `id` line, including its line break. */
	idSize: number;
	line: Uint8Array;
	lineSize: number;
	data: string;
	eventType: string;
	idBuffer: string;
	linePosition: LinePosition;
};

/** A complete frame; comments and cursor-only frames contain no event. */
export type SseFrame = { readonly event: SseEvent | null };
/** A frame overflow permanently stops this parser. An invalid offset does not consume input. */
export type SseParseError = {
	readonly kind: "frame-too-large" | "invalid-offset";
};
/** A bounded parser that consumes one frame at a time without an output queue. */
export type SseParser = {
	/** CRLF counts as one delimiter byte. EOF discards an incomplete frame. */
	readFrame(input: {
		readonly chunk: Uint8Array;
		readonly offset: number;
	}): Result<
		{ readonly offset: number; readonly frame: SseFrame | null },
		SseParseError
	>;
	getLastEventId(): string;
	/** The most recent retry value, or null; overflowing u64 values are ignored. */
	getRetryMillis(): bigint | null;
};

/** Creates a WHATWG SSE parser with an explicit frame bound in input bytes.
 * The bound counts one byte for each line break, and CRLF counts as one. It
 * applies to the current line together with the lines that the current frame
 * retains: every `data` line, the last `event` line, and the last `id` line
 * without NUL after the previous blank line, each with its line break. A
 * comment line or another field stops counting when its line ends, so comments
 * without blank lines do not accumulate toward the bound. Parsing fails when one
 * more input byte, including a line break, would exceed the bound.
 * UTF-8 replacement can expand a byte to three output bytes. IDs remain bounded
 * by a frame. Create a fresh parser for each connection. Supply the last committed
 * cursor when resuming a connection; omission starts without a prior cursor.
 * Initial cursors must be valid Unicode without NUL, CR, or LF and fit within
 * three times the frame byte bound, which permits decoded replacement characters.
 */
export function createSseParser(input: {
	readonly maxFrameSize: number;
	readonly initialEventId?: string;
}): Result<
	SseParser,
	{ readonly kind: "invalid-limit" | "invalid-initial-event-id" }
> {
	const maxSize = input.maxFrameSize;
	if (!Number.isSafeInteger(maxSize) || maxSize < 1) {
		return createErrorResult({ kind: "invalid-limit" });
	}
	const initialEventId = input.initialEventId ?? "";
	if (
		initialEventId.length > maxSize * 3 ||
		/[\0\r\n]/u.test(initialEventId)
	) {
		return createErrorResult({ kind: "invalid-initial-event-id" });
	}
	const encodedId = encodeUtf8(initialEventId, { invalidText: "reject" });
	if (encodedId.kind === "error" || encodedId.value.length > maxSize * 3) {
		return createErrorResult({ kind: "invalid-initial-event-id" });
	}
	let state:
		| { readonly kind: "reading"; readonly buffer: FrameBuffer }
		| { readonly kind: "failed" } = {
		kind: "reading",
		buffer: {
			dataSize: 0,
			eventTypeSize: 0,
			idSize: 0,
			line: new Uint8Array(Math.min(maxSize, 128)),
			lineSize: 0,
			data: "",
			eventType: "",
			idBuffer: initialEventId,
			linePosition: "initial",
		},
	};
	let lastEventId = initialEventId;
	let retryMillis: bigint | null = null;
	const decoder = new TextDecoder("utf-8", { ignoreBOM: true });
	function finishLine(buffer: FrameBuffer): SseFrame | null {
		const lineSizeWithBreak = buffer.lineSize + 1;
		let value = decoder.decode(buffer.line.subarray(0, buffer.lineSize));
		buffer.lineSize = 0;
		if (buffer.linePosition === "initial" && value.startsWith("\uFEFF")) {
			value = value.slice(1);
		}
		buffer.linePosition = "subsequent";
		if (value === "") {
			buffer.dataSize = 0;
			buffer.eventTypeSize = 0;
			buffer.idSize = 0;
			lastEventId = buffer.idBuffer;
			const event =
				buffer.data === ""
					? null
					: {
							eventType: buffer.eventType || "message",
							data: buffer.data.slice(0, -1),
							id: lastEventId,
						};
			buffer.data = "";
			buffer.eventType = "";
			return { event };
		}
		if (value.startsWith(":")) {
			return null;
		}
		const colon = value.indexOf(":");
		const field = colon < 0 ? value : value.slice(0, colon);
		let content = colon < 0 ? "" : value.slice(colon + 1);
		if (content.startsWith(" ")) {
			content = content.slice(1);
		}
		switch (field) {
			case "data":
				buffer.data += `${content}\n`;
				buffer.dataSize += lineSizeWithBreak;
				break;
			case "event":
				buffer.eventType = content;
				buffer.eventTypeSize = lineSizeWithBreak;
				break;
			case "id":
				if (!content.includes("\0")) {
					buffer.idBuffer = content;
					buffer.idSize = lineSizeWithBreak;
				}
				break;
			case "retry":
				// More than 20 nonzero decimal digits cannot fit u64. Avoid a
				// large BigInt conversion even when a caller permits large frames.
				if (/^[0-9]+$/u.test(content)) {
					const digits = content.replace(/^0+/u, "");
					if (digits.length <= 20) {
						const number = BigInt(digits || "0");
						if (number <= 18446744073709551615n) {
							retryMillis = number;
						}
					}
				}
				break;
		}
		return null;
	}
	return createSuccessResult({
		getLastEventId: () => lastEventId,
		getRetryMillis: () => retryMillis,
		readFrame({ chunk, offset }) {
			if (
				!Number.isSafeInteger(offset) ||
				offset < 0 ||
				offset > chunk.length
			) {
				return createErrorResult({ kind: "invalid-offset" });
			}
			if (state.kind === "failed") {
				return createErrorResult({ kind: "frame-too-large" });
			}
			const { buffer } = state;
			while (offset < chunk.length) {
				const byte = chunk[offset]!;
				offset += 1;
				if (buffer.linePosition === "after-cr") {
					buffer.linePosition = "subsequent";
					if (byte === 10) {
						continue;
					}
				}
				if (
					buffer.dataSize +
						buffer.eventTypeSize +
						buffer.idSize +
						buffer.lineSize ===
					maxSize
				) {
					state = { kind: "failed" };
					return createErrorResult({ kind: "frame-too-large" });
				}
				if (byte === 10 || byte === 13) {
					const frame = finishLine(buffer);
					buffer.linePosition =
						byte === 13 ? "after-cr" : "subsequent";
					if (frame !== null) {
						return createSuccessResult({ offset, frame });
					}
				} else {
					if (buffer.lineSize === buffer.line.length) {
						const expanded = new Uint8Array(
							Math.min(maxSize, buffer.line.length * 2),
						);
						expanded.set(buffer.line);
						buffer.line = expanded;
					}
					buffer.line[buffer.lineSize] = byte;
					buffer.lineSize += 1;
				}
			}
			return createSuccessResult({ offset, frame: null });
		},
	});
}
