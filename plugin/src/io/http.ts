// HTTP: в Obsidian — только requestUrl (fetch на десктопе упирается в CORS), в
// тестах — fetch из Node.

import type { HttpBackend, HttpResponse } from "./backend.ts";

type RequestUrl = (p: {
  url: string;
  method: string;
  headers: Record<string, string>;
  body?: ArrayBuffer;
  contentType?: string;
  throw: boolean;
}) => Promise<{ status: number; headers: Record<string, string>; arrayBuffer: ArrayBuffer }>;

function headerRecord(headers: [string, string][]): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [k, v] of headers) out[k] = v;
  return out;
}

function body(data: Uint8Array): ArrayBuffer | undefined {
  if (data.length === 0) return undefined;
  return data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength) as ArrayBuffer;
}

/**
 * requestUrl не умеет таймаут и отмену: гонка с таймером, чтобы зависший запрос не
 * держал цикл синка (сам запрос доживёт в фоне, его результат игнорируется).
 */
export class ObsidianHttp implements HttpBackend {
  constructor(private requestUrl: RequestUrl) {}

  async request(url: string, method: string, headers: [string, string][], data: Uint8Array, timeoutMs: number): Promise<HttpResponse> {
    const rec = headerRecord(headers);
    const ct = Object.entries(rec).find(([k]) => k.toLowerCase() === "content-type")?.[1];
    const req = this.requestUrl({ url, method, headers: rec, body: body(data), contentType: ct, throw: false });
    let timer: ReturnType<typeof setTimeout> | undefined;
    const timeout = new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error(`таймаут ${timeoutMs} мс`)), timeoutMs);
    });
    try {
      const r = await Promise.race([req, timeout]);
      return {
        status: r.status,
        headers: Object.entries(r.headers).map(([k, v]) => [k.toLowerCase(), v]),
        body: new Uint8Array(r.arrayBuffer),
      };
    } finally {
      clearTimeout(timer);
    }
  }
}

export class FetchHttp implements HttpBackend {
  async request(url: string, method: string, headers: [string, string][], data: Uint8Array, timeoutMs: number): Promise<HttpResponse> {
    const r = await fetch(url, {
      method,
      headers: headerRecord(headers),
      body: body(data),
      signal: AbortSignal.timeout(timeoutMs),
    });
    const out: [string, string][] = [];
    r.headers.forEach((v, k) => out.push([k, v]));
    return { status: r.status, headers: out, body: new Uint8Array(await r.arrayBuffer()) };
  }
}
