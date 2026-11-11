import { Result } from "effect";

declare const URL_PATHNAME_BRAND: unique symbol;

export type UrlPathname = string & {
	readonly [URL_PATHNAME_BRAND]: true;
};

export type UrlPathnameParseIssue = {
	readonly _tag: "UrlPathnameParseIssue";
	readonly value: string;
	readonly cause?: unknown;
};

const URL_PATHNAME_PARSER_ORIGIN = "https://vorma.invalid";

type ParsedHref = {
	readonly origin: string;
	readonly pathname: string;
	readonly search: string;
	readonly hash: string;
};

function parseHref(input: {
	readonly value: string;
	readonly base: string;
}): ParsedHref {
	return new URL(input.value, input.base);
}

/** Parses the exact pathname representation used for request classification. */
export function parseUrlPathname(
	value: string,
): Result.Result<UrlPathname, UrlPathnameParseIssue> {
	const parsed = Result.try({
		try: () => parseHref({ value, base: URL_PATHNAME_PARSER_ORIGIN }),
		catch: (cause) => createUrlPathnameParseError({ value, cause }),
	});
	if (Result.isFailure(parsed)) {
		return Result.fail(parsed.failure);
	}
	if (
		!value.startsWith("/") ||
		parsed.success.origin !== URL_PATHNAME_PARSER_ORIGIN ||
		parsed.success.pathname !== value ||
		parsed.success.search !== "" ||
		parsed.success.hash !== ""
	) {
		return Result.fail(createUrlPathnameParseError({ value }));
	}
	return Result.succeed(value as UrlPathname);
}

function createUrlPathnameParseError(input: {
	readonly value: string;
	readonly cause?: unknown;
}): UrlPathnameParseIssue {
	return {
		_tag: "UrlPathnameParseIssue",
		value: input.value,
		...(input.cause === undefined ? {} : { cause: input.cause }),
	};
}
