import { describe, expect, test } from "bun:test";
import { flatten, UNKNOWN_SERVICE } from "./otlp";

const SAMPLE = {
  resourceSpans: [{
    resource: { attributes: [{ key: "service.name", value: { stringValue: "checkout" } }] },
    scopeSpans: [{
      scope: { name: "example.http" },
      spans: [
        {
          traceId: "5B8EFFF798038103D269B633813FC60C", spanId: "EEE19B7EC3C1B174", parentSpanId: "",
          name: "GET /cart", kind: 2, startTimeUnixNano: "1700000000000000000", endTimeUnixNano: "1700000000250000000",
          attributes: [
            { key: "http.status_code", value: { intValue: "200" } },
            { key: "http.route", value: { stringValue: "/cart" } },
            { key: "tags", value: { arrayValue: { values: [{ stringValue: "a" }, { boolValue: true }] } } },
          ],
          status: { code: "STATUS_CODE_OK" },
        },
        {
          traceId: "5b8efff798038103d269b633813fc60c", spanId: "0f3d1f1cc9f0a8a1", parentSpanId: "eee19b7ec3c1b174",
          name: "SELECT cart", kind: "SPAN_KIND_CLIENT", startTimeUnixNano: 1700000000010000000, endTimeUnixNano: 1700000000020000000,
          status: { code: 2, message: "timeout" },
        },
        { traceId: "not-hex", spanId: "0f3d1f1cc9f0a8a1", name: "broken", startTimeUnixNano: "1", endTimeUnixNano: "2" },
      ],
    }],
  }],
};

describe("flatten", () => {
  test("flattens the sample request and rejects the bad span", () => {
    const flat = flatten(SAMPLE);
    expect(flat.rejected).toBe(1);
    expect(flat.rows).toHaveLength(2);
    const [root, child] = flat.rows;
    expect(root!.traceId).toBe("5b8efff798038103d269b633813fc60c");
    expect(root!.spanId).toBe("eee19b7ec3c1b174");
    expect(root!.parentSpanId).toBeNull();
    expect(root!.serviceName).toBe("checkout");
    expect(root!.scopeName).toBe("example.http");
    expect(root!.kind).toBe(2);
    expect(root!.statusCode).toBe(1);
    expect(root!.endNs - root!.startNs).toBe(250_000_000n);
    expect(JSON.parse(root!.attributesJson)).toEqual({ "http.status_code": 200, "http.route": "/cart", tags: ["a", true] });
    expect(child!.parentSpanId).toBe("eee19b7ec3c1b174");
    expect(child!.kind).toBe(3);
    expect(child!.statusCode).toBe(2);
    expect(child!.statusMessage).toBe("timeout");
    expect(child!.attributesJson).toBe("{}");
  });

  test("falls back to unknown_service and rejects zero ids / inverted timestamps", () => {
    const ok = flatten({ resourceSpans: [{ scopeSpans: [{ spans: [{ traceId: "0102030405060708090a0b0c0d0e0f10", spanId: "0102030405060708", startTimeUnixNano: "5", endTimeUnixNano: "6" }] }] }] });
    expect(ok.rows[0]!.serviceName).toBe(UNKNOWN_SERVICE);
    const bad = flatten({ resourceSpans: [{ scopeSpans: [{ spans: [
      { traceId: "00000000000000000000000000000000", spanId: "0102030405060708", startTimeUnixNano: "5", endTimeUnixNano: "6" },
      { traceId: "0102030405060708090a0b0c0d0e0f10", spanId: "0102030405060708", startTimeUnixNano: "10", endTimeUnixNano: "5" },
    ] }] }] });
    expect(bad.rows).toHaveLength(0);
    expect(bad.rejected).toBe(2);
  });

  test("garbage input is harmless", () => {
    expect(flatten(null)).toEqual({ rows: [], rejected: 0 });
    expect(flatten({ resourceSpans: "nope" })).toEqual({ rows: [], rejected: 0 });
    expect(flatten({ resourceSpans: [{ scopeSpans: [{ spans: [42] }] }] }).rejected).toBe(1);
  });
});
