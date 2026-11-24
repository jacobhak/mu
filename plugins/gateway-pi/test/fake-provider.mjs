// Test-only pi extension: points pi's built-in anthropic provider (catalog kept) at the fake
// upstream named by MU_FAKE_UPSTREAM, with a dummy key.
export default function (pi) {
  pi.registerProvider("anthropic", { baseUrl: process.env.MU_FAKE_UPSTREAM, apiKey: "k" });
}
