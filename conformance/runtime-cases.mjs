const clients = {
  "mpp-buyer": ["POST", "/v1/transactions/mpp"],
  "mpp-seller": ["GET", "/v1/mpp/config"],
  "x402-buyer": ["GET", "/v1/transactions/x402-supported"],
  "x402-seller": ["GET", "/v1/x402/config"],
};

export function runtimeCases(scenarios) {
  const cases = [];
  for (const [product, [method, path]] of Object.entries(clients)) {
    for (const [name, options, origin] of [
      ["default", {}, "https://api.inflowpay.ai"],
      ["production", { environment: "production" }, "https://api.inflowpay.ai"],
      ["sandbox", { environment: "sandbox" }, "https://sandbox.inflowpay.ai"],
    ])
      cases.push({
        id: `${product}.environment.${name}`,
        suite: "runtime",
        operation: "runtime.environment",
        input: { product, api_key: "test-only-key", ...options },
        expect: { result: { destinations: [`${method} ${origin}${path}`] } },
      });
    const request = (headers) => ({
      method,
      path,
      headers,
      ...(method === "POST"
        ? {
            json: {
              challenge: {
                id: "test",
                realm: "seller.example",
                method: "inflow",
                intent: "charge",
                request: Buffer.from(
                  JSON.stringify({ amount: "1", currency: "USD" }),
                ).toString("base64url"),
              },
              options: {},
            },
          }
        : {}),
    });
    const add = (id, input, exchanges, response) => {
      const error = response.json?.errors?.[0];
      cases.push({
        id: `${product}.${id}`,
        suite: "runtime",
        operation: "runtime.request",
        input: { product, ...input },
        platform: { exchanges },
        expect: {
          result: {
            code: error?.code ?? "UNEXPECTED_ERROR",
            message: error?.message ?? "request failed",
            http_status: response.status,
            endpoint: path,
            token_calls: input.tokens?.length ?? 0,
            request_id: response.headers?.["x-request-id"] ?? "",
            sensitive_headers: [],
          },
        },
      });
    };
    for (const [id, scenario] of Object.entries(scenarios)) {
      if (!id.startsWith("auth.")) continue;
      const { request: original, response } = scenario.exchanges[0];
      if (response.status < 400) continue;
      if (product === "x402-seller" && !original.headers["x-api-key"]) continue;
      if (id.startsWith("auth.seller-required") && !product.endsWith("seller"))
        continue;
      const headers = original.headers;
      const input = headers.authorization
        ? { tokens: [headers.authorization.slice(7)] }
        : headers["x-api-key"]
          ? { api_key: headers["x-api-key"] }
          : {};
      add(id, input, [{ request: request(headers), response }], response);
    }
    for (const status of [
      301, 302, 303, 307, 308, 400, 401, 403, 404, 409, 412, 500,
    ]) {
      const response = {
        status,
        headers: {
          location: "/must-not-follow",
          "x-request-id": "test-request",
          "set-cookie": "test-only-secret",
        },
      };
      const exchanges = [
        { request: request({ "x-api-key": "test-only-key" }), response },
      ];
      add(`http.${status}`, { api_key: "test-only-key" }, exchanges, response);
    }
    const exchanges = [
      {
        request: request({ "x-api-key": "test-only-key" }),
        response: { status: 503 },
      },
    ];
    if (method === "GET")
      exchanges.push({
        request: request({ "x-api-key": "test-only-key" }),
        response: { status: 401 },
      });
    add(
      "retry-policy",
      { api_key: "test-only-key" },
      exchanges,
      exchanges.at(-1).response,
    );
  }
  return { cases };
}
