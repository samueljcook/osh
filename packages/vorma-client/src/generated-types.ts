import type { PATTERN_PROTOCOL } from "./protocol/constants.ts";

/*
Type-level readers of the application's generated types. The build writes a
`declare module "@vorma/client"` augmentation of `VormaGeneratedTypes` with the
fields `views`, `apiEndpoints`, and `assets`; no application file imports it.
Before the first build the interface is empty: patterns accept any string and
outputs are `any`. A client project type-checks in that state only as far as
`any` allows: under `noImplicitAny`, a callback over an output of type `any`
has parameters without a type, and under `noUncheckedIndexedAccess`,
`staticPublicAssets[name]` is `string | undefined`. A project with those
options runs the build before its typecheck.

`views` maps a pattern to `{ input; output; search; clientLoader: { success };
errors; childPatterns }`. `apiEndpoints` maps `"METHOD /pattern"` to
`{ kind; input; output; errors }`. `assets` is the union of static public asset names.
*/

type DynamicParamPrefix = (typeof PATTERN_PROTOCOL)["dynamicParamPrefix"];
type SplatSegment = (typeof PATTERN_PROTOCOL)["splatSegment"];
type IndexSegment = (typeof PATTERN_PROTOCOL)["indexSegment"];

/** Augmented by the build's generated file. */
// oxlint-disable-next-line typescript/no-empty-object-type
export interface VormaGeneratedTypes {}

type UntypedView = {
	readonly input: any;
	readonly output: any;
	readonly search: any;
	readonly clientLoader: { readonly success: any };
	readonly errors: Readonly<Record<string, any>>;
	readonly childPatterns: string;
};

type UntypedApiEndpoint = {
	readonly kind: "query" | "mutation";
	readonly input: any;
	readonly output: any;
	readonly errors: Readonly<Record<string, any>>;
};

export type ViewContracts = VormaGeneratedTypes extends {
	readonly views: infer Views;
}
	? Views
	: Readonly<Record<string, UntypedView>>;

export type ApiEndpointContracts = VormaGeneratedTypes extends {
	readonly apiEndpoints: infer ApiEndpoints;
}
	? ApiEndpoints
	: Readonly<Record<string, UntypedApiEndpoint>>;

/** The names of the application's static public assets, as relative paths without a leading slash. */
export type StaticPublicAssetName = VormaGeneratedTypes extends {
	readonly assets: infer Asset extends string;
}
	? Asset
	: string;

export type ViewPattern = Extract<keyof ViewContracts, string>;

export type ParamName<Pattern extends string> =
	Pattern extends `${infer Segment}/${infer Rest}`
		? SegmentParamName<Segment> | ParamName<Rest>
		: SegmentParamName<Pattern>;

type SegmentParamName<Segment extends string> =
	Segment extends `${DynamicParamPrefix}${infer Name}` ? Name : never;

type ViewParamsOf<Pattern extends string> = string extends Pattern
	? Readonly<Record<string, string>>
	: Readonly<Record<ParamName<Pattern>, string>>;

/** The `params` field, required exactly when the pattern declares dynamic params. */
type ParamsField<Pattern extends string> = string extends Pattern
	? { readonly params?: Readonly<Record<string, string>> }
	: [ParamName<Pattern>] extends [never]
		? { readonly params?: never }
		: {
				readonly params: {
					readonly [Name in ParamName<Pattern>]: string;
				};
			};

/** The `splatValues` field, required exactly when the pattern ends in a splat. */
type SplatField<Pattern extends string> = string extends Pattern
	? { readonly splatValues?: ReadonlyArray<string> }
	: Pattern extends `${string}/${SplatSegment}`
		? { readonly splatValues: ReadonlyArray<string> }
		: { readonly splatValues?: never };

/** The patterns that address a page: an explicit index page is also addressed by its layout's pattern. */
type DestinationPattern<Pattern extends string> =
	Pattern extends `${infer Prefix}/${IndexSegment}`
		? Pattern | (Prefix extends "" ? "/" : Prefix)
		: Pattern;

type RequiredKeys<Value> = Value extends object
	? {
			[Key in keyof Value]-?: {} extends Pick<Value, Key> ? never : Key;
		}[keyof Value]
	: never;

type IsAny<Value> = 0 extends 1 & Value ? true : false;

/** The `input` field: absent for a page without search input, and optional for an application without generated types. */
type InputField<Search> =
	IsAny<Search> extends true
		? { readonly input?: unknown }
		: [Search] extends [undefined]
			? { readonly input?: never }
			: RequiredKeys<Search> extends never
				? { readonly input?: Search }
				: { readonly input: Search };

/**
 * The search input of a whole page: the composed input of every loader at or
 * above the page. Targets that write a URL take it.
 */
export type ViewSearchOf<Pattern extends ViewPattern> =
	ViewContracts[Pattern] extends {
		readonly search: infer Search;
	}
		? Search
		: undefined;

/**
 * The search input of one view's own loaders, decoded with that view's own
 * contract; `undefined` for a view without a loader. Every read of a view's
 * input has this type.
 */
export type ViewInputOf<Pattern extends ViewPattern> =
	ViewContracts[Pattern] extends { readonly input: infer Input }
		? Input
		: undefined;

/** The server loader output of a page; `undefined` for a page without a loader. */
export type ViewServerOutputOf<Pattern extends ViewPattern> =
	ViewContracts[Pattern] extends { readonly output: infer Output }
		? Output
		: never;

/** The client loader output of a page; `undefined` for a page without a client loader. */
export type ViewClientOutputOf<Pattern extends ViewPattern> =
	ViewContracts[Pattern] extends {
		readonly clientLoader: { readonly success: infer Success };
	}
		? Success
		: undefined;

/**
 * A value or `undefined`. Output types are `any` before the first build, and
 * a union written directly with `any` collapses to `any`, so output types that
 * can be absent go through this generic alias.
 */
export type OrUndefined<Value> = Value | undefined;

/** The patterns for pages with a server loader. */
export type ViewServerLoaderPattern = {
	[Pattern in ViewPattern]: IsAny<ViewServerOutputOf<Pattern>> extends true
		? Pattern
		: [ViewServerOutputOf<Pattern>] extends [undefined]
			? never
			: Pattern;
}[ViewPattern];

/** The declared errors of the server loaders of one or more pages. */
export type ViewDeclaredErrorOf<Pattern extends ViewPattern> = {
	[Each in Pattern]: ViewContracts[Each] extends {
		readonly errors: infer Errors;
	}
		? Errors[keyof Errors]
		: never;
}[Pattern];

/** The declared errors of a page's immediate children's loaders. */
export type ViewChildDeclaredErrorOf<Pattern extends ViewPattern> = {
	[Each in Pattern]: ViewContracts[Each] extends {
		readonly childPatterns: infer Children extends ViewPattern;
	}
		? ViewDeclaredErrorOf<Children>
		: never;
}[Pattern];

/** One page's facts as a view or the state reads them. */
export type PatternMatch<Pattern extends ViewPattern = ViewPattern> = {
	readonly pattern: Pattern;
	readonly params: ViewParamsOf<Pattern>;
	readonly splatValues: ReadonlyArray<string>;
	readonly input: ViewInputOf<Pattern>;
	readonly serverOutput: ViewServerOutputOf<Pattern>;
	/** Undefined when the view has no client loader, or its loader failed or did not run. */
	readonly clientOutput: OrUndefined<ViewClientOutputOf<Pattern>>;
};

export type AnyPatternMatch = {
	[Pattern in ViewPattern]: PatternMatch<Pattern>;
}[ViewPattern];

/** A page given by one of the application's patterns, with its params, splat values, search input, and hash. */
export type PageTarget<Pattern extends ViewPattern = ViewPattern> =
	Pattern extends string
		? {
				readonly href?: never;
				readonly pattern: DestinationPattern<Pattern>;
				readonly hash?: string;
			} & ParamsField<Pattern> &
				SplatField<Pattern> &
				InputField<ViewSearchOf<Pattern>>
		: never;

/** A target given by a raw href, which the build does not check. */
export type HrefTarget = {
	readonly href: string;
	readonly pattern?: never;
	readonly params?: never;
	readonly splatValues?: never;
	readonly input?: never;
	readonly hash?: never;
};

/** What `navigate` and `Link` accept: a page target or a raw href. */
export type NavTarget = PageTarget | HrefTarget;

/** The runtime form of a target. */
export type RawTarget =
	| HrefTarget
	| {
			readonly href?: never;
			readonly pattern: string;
			readonly params?: Readonly<Record<string, string>> | undefined;
			readonly splatValues?: ReadonlyArray<string> | undefined;
			readonly input?: unknown;
			readonly hash?: string | undefined;
	  };

/** Endpoint request options, which the platform's `fetch` takes. */
export type ApiEndpointRequestOptions = Omit<
	RequestInit,
	"method" | "body" | "signal"
>;

type ApiEndpointKind = "query" | "mutation";

/**
 * The input of a call. A form endpoint takes `formInput`: the form's fields
 * as an object, or a `FormData`. Every other endpoint with an input takes
 * `input`.
 */
type ApiEndpointInputField<Contract, Method extends string> = Contract extends {
	readonly formInput: infer Fields;
}
	? { readonly formInput: Fields | FormData; readonly input?: never }
	: Contract extends { readonly input: infer Input }
		? [Input] extends [undefined]
			? { readonly input?: never; readonly formInput?: never }
			: {
					readonly input:
						| Input
						| (Method extends "GET" ? FormData : never);
					readonly formInput?: never;
				}
		: { readonly input?: never; readonly formInput?: never };

type ApiEndpointCommon = {
	readonly signal?: AbortSignal;
	readonly skipWorkIndicator?: boolean;
	readonly requestOptions?: ApiEndpointRequestOptions;
	/** A later call with the same key cancels this call while it is unfinished. */
	readonly supersedeKey?: string;
};

type ApiEndpointCallFor<Kind extends ApiEndpointKind> =
	string extends Extract<keyof ApiEndpointContracts, string>
		? ApiEndpointCommon & {
				readonly kind: Kind;
				readonly method: string;
				readonly pattern: string;
				readonly params?: Readonly<Record<string, string>>;
				readonly splatValues?: ReadonlyArray<string>;
				readonly input?: unknown;
				readonly formInput?: unknown;
			} & (Kind extends "mutation"
					? { readonly skipAutoRevalidate?: boolean }
					: {})
		: {
				[
					Identity in Extract<keyof ApiEndpointContracts, string>
				]: ApiEndpointContracts[Identity] extends {
					readonly kind: Kind;
				}
					? Identity extends `${infer Method} ${infer Pattern}`
						? ApiEndpointCommon & {
								readonly kind: Kind;
								readonly method: Method;
								readonly pattern: Pattern;
							} & (Kind extends "mutation"
									? { readonly skipAutoRevalidate?: boolean }
									: {}) &
								ParamsField<Pattern> &
								SplatField<Pattern> &
								ApiEndpointInputField<
									ApiEndpointContracts[Identity],
									Method
								>
						: never
					: never;
			}[Extract<keyof ApiEndpointContracts, string>];

export type QueryApiEndpointCall = ApiEndpointCallFor<"query">;
export type MutationApiEndpointCall = ApiEndpointCallFor<"mutation">;
export type AnyApiEndpointCall = QueryApiEndpointCall | MutationApiEndpointCall;

/**
 * Turns every property of `Call` that its own endpoint declaration does not
 * take into `never`. A generic parameter hides excess properties from
 * TypeScript's own check, so a call is checked against its declaration's
 * property names instead. For example, `skipAutoRevalidate` belongs to
 * mutations only, and `body` belongs to no call.
 */
export type RejectUndeclaredOptions<Call extends AnyApiEndpointCall> =
	Call extends unknown
		? {
				readonly [
					Name in Exclude<keyof Call, OptionNamesOfDeclaration<Call>>
				]: never;
			}
		: never;

type ApiEndpointIdentityNames = "kind" | "method" | "pattern";

type OptionNamesOfDeclaration<Call extends AnyApiEndpointCall> =
	AnyApiEndpointCall extends infer Declaration extends AnyApiEndpointCall
		? Declaration extends unknown
			? Pick<Call, ApiEndpointIdentityNames> extends Pick<
					Declaration,
					ApiEndpointIdentityNames
				>
				? keyof Declaration
				: never
			: never
		: never;

/** The runtime form of an endpoint call. */
export type RawApiEndpointCall = ApiEndpointCommon & {
	readonly kind: ApiEndpointKind;
	readonly method: string;
	readonly pattern: string;
	readonly params?: Readonly<Record<string, string>> | undefined;
	readonly splatValues?: ReadonlyArray<string> | undefined;
	readonly input?: unknown;
	readonly formInput?: unknown;
	readonly skipAutoRevalidate?: boolean;
};

type ContractOf<Call extends { method: string; pattern: string }> =
	`${Call["method"]} ${Call["pattern"]}` extends infer Identity extends
		keyof ApiEndpointContracts
		? ApiEndpointContracts[Identity]
		: never;

export type ApiEndpointOutputOf<
	Call extends { method: string; pattern: string },
> = ContractOf<Call> extends { readonly output: infer Output } ? Output : any;

export type ApiEndpointDeclaredErrorOf<
	Call extends { method: string; pattern: string },
> =
	ContractOf<Call> extends { readonly errors: infer Errors }
		? Errors[keyof Errors]
		: any;
