/** An SSE data event. */
export type SseEvent = {
	readonly eventType: string;
	readonly data: string;
	/**
	 * The stream's committed cursor after this event's frame: the value of the
	 * frame's last `id` field without NUL, or the previous cursor when the frame
	 * has no such field. An empty string means that the stream has no cursor.
	 */
	readonly id: string;
};
