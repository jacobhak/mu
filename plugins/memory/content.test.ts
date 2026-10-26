import { expect, test } from "bun:test";
import { truncateApproxTokens, truncateCodePoints } from "./content";

test("never splits a surrogate pair", () => {
	const s = "ab😀cd"; // 😀 is two UTF-16 units at index 2..3
	expect(truncateCodePoints(s, 3)).toBe("ab");
	expect(truncateCodePoints(s, 4)).toBe("ab😀");
	expect(truncateCodePoints(s, 100)).toBe(s);
	expect(truncateCodePoints(s, 0)).toBe("");
});

test("approximate token truncation keeps valid text", () => {
	const out = truncateApproxTokens("😀".repeat(50), 5); // 20 chars budget
	expect(out.endsWith("…")).toBe(true);
	expect(/[\ud800-\udbff](?![\udc00-\udfff])/.test(out)).toBe(false);
});
