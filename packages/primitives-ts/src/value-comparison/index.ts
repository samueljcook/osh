type ValuePair = {
	readonly left: unknown;
	readonly right: unknown;
	readonly requireIdentity: boolean;
};

// Clock replacements can change global Date while structuredClone still creates native dates.
const datePrototype = Date.prototype;

const regExpStateDescriptors = Object.entries(
	Object.getOwnPropertyDescriptors(RegExp.prototype),
).filter(
	([name, descriptor]) => name !== "flags" && descriptor.get !== undefined,
);

/**
 * This function uses Object.is for primitives and identity for unrecognized
 * objects. Arrays and plain objects compare their own enumerable string and
 * symbol properties, without regard to property order. Array holes differ from
 * explicit undefined. Prototypes must match.
 *
 * Dates, URLs, and regular expressions compare their built-in state and
 * enumerable properties. Regular-expression state includes lastIndex. Search
 * parameters (`URLSearchParams`), headers (`Headers`), and form data
 * (`FormData`) compare their entries in iteration order, by name and by value,
 * and their enumerable properties; a form-data file value compares by
 * identity, and headers iterate in the sorted, combined order that `Headers`
 * defines.
 * Maps and sets retain iteration order; map keys and set members use identity,
 * while map values compare recursively. Supported containers must use this
 * realm's built-in prototypes; other objects and subclasses use identity.
 *
 * Cycles and shared references within inspected containers must have matching
 * relationships. Distinct containers with enumerable accessors compare unequal;
 * getters are not invoked.
 * Non-enumerable custom properties and property mutability are not compared.
 * Property reflection can invoke proxy traps; exceptions from those traps propagate.
 * Inputs must remain unchanged during comparison. The operation uses an explicit
 * work stack, so nesting does not consume the JavaScript call stack.
 */
export function compareValuesForStructuralEquality(input: {
	readonly left: unknown;
	readonly right: unknown;
}): boolean {
	if (Object.is(input.left, input.right)) {
		return true;
	}
	const pending: Array<ValuePair> = [{ ...input, requireIdentity: false }];
	const leftToRight = new WeakMap<object, object>();
	const rightToLeft = new WeakMap<object, object>();
	while (pending.length !== 0) {
		const { left, right, requireIdentity } = pending.pop()!;
		const sameIdentity = Object.is(left, right);
		if (requireIdentity && !sameIdentity) {
			return false;
		}
		if (
			typeof left !== "object" ||
			left === null ||
			typeof right !== "object" ||
			right === null
		) {
			if (!sameIdentity) {
				return false;
			}
			continue;
		}
		if (leftToRight.has(left) || rightToLeft.has(right)) {
			if (
				leftToRight.get(left) !== right ||
				rightToLeft.get(right) !== left
			) {
				return false;
			}
			continue;
		}
		leftToRight.set(left, right);
		rightToLeft.set(right, left);
		const prototype = Object.getPrototypeOf(left);
		if (prototype !== Object.getPrototypeOf(right)) {
			return false;
		}
		if (Array.isArray(left) && prototype === Array.prototype) {
			if (!Array.isArray(right) || left.length !== right.length) {
				return false;
			}
		} else if (prototype === datePrototype) {
			if (
				!Object.is(
					datePrototype.getTime.call(left),
					datePrototype.getTime.call(right),
				)
			) {
				return false;
			}
		} else if (prototype === URL.prototype) {
			if (
				URL.prototype.toString.call(left) !==
				URL.prototype.toString.call(right)
			) {
				return false;
			}
		} else if (
			prototype === URLSearchParams.prototype ||
			prototype === Headers.prototype ||
			prototype === FormData.prototype
		) {
			const leftEntries = readEntryList(left as EntryList);
			const rightEntries = readEntryList(right as EntryList);
			if (leftEntries.length !== rightEntries.length) {
				return false;
			}
			for (const [index, [name, value]] of leftEntries.entries()) {
				const rightEntry = rightEntries[index];
				if (rightEntry === undefined || rightEntry[0] !== name) {
					return false;
				}
				pending.push({
					left: value,
					right: rightEntry[1],
					requireIdentity: false,
				});
			}
		} else if (prototype === RegExp.prototype) {
			const leftRegExp = left as RegExp;
			const rightRegExp = right as RegExp;
			if (
				!regExpStateDescriptors.every(([, descriptor]) => {
					return Object.is(
						descriptor.get!.call(leftRegExp),
						descriptor.get!.call(rightRegExp),
					);
				})
			) {
				return false;
			}
			pending.push({
				left: leftRegExp.lastIndex,
				right: rightRegExp.lastIndex,
				requireIdentity: false,
			});
		} else if (prototype === Map.prototype) {
			const leftEntries = Map.prototype.entries.call(left) as MapIterator<
				[unknown, unknown]
			>;
			const rightEntries = Map.prototype.entries.call(
				right,
			) as MapIterator<[unknown, unknown]>;
			while (true) {
				const leftEntry = leftEntries.next();
				const rightEntry = rightEntries.next();
				if (leftEntry.done || rightEntry.done) {
					if (leftEntry.done !== rightEntry.done) {
						return false;
					}
					break;
				}
				pending.push({
					left: leftEntry.value[0],
					right: rightEntry.value[0],
					requireIdentity: true,
				});
				pending.push({
					left: leftEntry.value[1],
					right: rightEntry.value[1],
					requireIdentity: false,
				});
			}
		} else if (prototype === Set.prototype) {
			const leftValues = Set.prototype.values.call(
				left,
			) as SetIterator<unknown>;
			const rightValues = Set.prototype.values.call(
				right,
			) as SetIterator<unknown>;
			while (true) {
				const leftMember = leftValues.next();
				const rightMember = rightValues.next();
				if (leftMember.done || rightMember.done) {
					if (leftMember.done !== rightMember.done) {
						return false;
					}
					break;
				}
				pending.push({
					left: leftMember.value,
					right: rightMember.value,
					requireIdentity: true,
				});
			}
		} else if (prototype !== Object.prototype && prototype !== null) {
			if (!sameIdentity) {
				return false;
			}
			continue;
		}
		const leftKeys = getEnumerableKeys(left);
		if (leftKeys.length !== getEnumerableKeys(right).length) {
			return false;
		}
		for (const key of leftKeys) {
			const leftDescriptor = Object.getOwnPropertyDescriptor(left, key)!;
			const rightDescriptor = Object.getOwnPropertyDescriptor(right, key);
			if (
				rightDescriptor?.enumerable !== true ||
				"value" in leftDescriptor !== "value" in rightDescriptor
			) {
				return false;
			}
			if ("value" in leftDescriptor) {
				pending.push({
					left: leftDescriptor.value,
					right: rightDescriptor.value,
					requireIdentity: false,
				});
			} else if (!sameIdentity) {
				return false;
			}
		}
	}
	return true;
}

type EntryList = URLSearchParams | Headers | FormData;

/** The entries of search parameters, headers, or form data, in iteration order, through the built-in method of the value's own type. */
function readEntryList(value: EntryList): Array<readonly [string, unknown]> {
	const entries: Array<readonly [string, unknown]> = [];
	function collect(entryValue: unknown, name: string): void {
		entries.push([name, entryValue]);
	}
	if (value instanceof URLSearchParams) {
		URLSearchParams.prototype.forEach.call(value, collect);
	} else if (value instanceof Headers) {
		Headers.prototype.forEach.call(value, collect);
	} else {
		FormData.prototype.forEach.call(value, collect);
	}
	return entries;
}

function getEnumerableKeys(value: object): Array<PropertyKey> {
	return Reflect.ownKeys(value).filter((key) =>
		Object.prototype.propertyIsEnumerable.call(value, key),
	);
}
