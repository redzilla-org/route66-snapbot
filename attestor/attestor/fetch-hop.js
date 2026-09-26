// ============================================================================
// IPv6 fetch hop for capture-http-raw (redzilla-org/route66#3659).
//
// WHY A SEPARATE FUNCTION: the prod rendezvous health origins
// (rendezvous-service-origin.<brand>:7080) publish AAAA records only and serve
// plain http. A Lambda outside a VPC has IPv4-only egress, so the signer cannot
// reach them. Attaching the SIGNER to a VPC would cost it its IPv4 internet
// (GitHub publication, STS, SSM, Step Functions) unless a NAT gateway were added,
// and the owner's order was "enable the ipv6 egress, with no additional costs".
// So this one function runs in a dual-stack VPC whose only internet route is an
// egress-only internet gateway (::/0), and it does exactly ONE thing: one GET
// hop, no redirect following, bytes returned as served.
//
// WHY THIS KEEPS "the attestor should only sign what it retrieves" (owner
// 2026-08-26): this function is declared in the snapbot stack, runs the same
// immutable snapbot image, and is invocable only by the signer's own role. The
// retrieval therefore stays inside snapbot's own trust boundary; the signer
// never signs bytes a caller handed it. It holds no key and writes nothing.
// ============================================================================

"use strict";

const http = require("http");
const { URL } = require("url");

exports.handler = async (event) => {
  const u = new URL(String(event.url || ""));
  // http only: https hops are made in-process by the signer, over IPv4.
  if (u.protocol !== "http:") throw new Error("fetch-hop serves http:// hops only, got " + u.protocol);
  const timeoutMS = Math.max(1000, Math.min(30000, Number(event.timeout_ms || 15000)));
  return await new Promise((resolve, reject) => {
    const req = http.request({
      method: "GET",
      hostname: u.hostname,
      port: u.port || 80,
      path: u.pathname + u.search,
      headers: event.headers || {},
    }, (res) => {
      // The peer address is read before the body so a closed socket cannot hide
      // it; it is the proof the hop went out over IPv6.
      const remoteAddress = (res.socket && res.socket.remoteAddress) || "";
      const remotePort = (res.socket && res.socket.remotePort) || 0;
      const chunks = [];
      res.on("data", (c) => chunks.push(c));
      res.on("error", reject);
      res.on("end", () => resolve({
        status: res.statusCode || 0,
        headers: res.headers || {},
        body_b64: Buffer.concat(chunks).toString("base64"),
        remote_address: remoteAddress,
        remote_port: remotePort,
      }));
    });
    req.setTimeout(timeoutMS, () => req.destroy(new Error("timeout after " + timeoutMS + "ms requesting " + u.toString())));
    req.on("error", reject);
    req.end();
  });
};
