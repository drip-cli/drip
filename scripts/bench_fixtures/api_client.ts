import { z } from "zod";
import { setTimeout as delay } from "node:timers/promises";
import { performance } from "node:perf_hooks";
import { randomUUID } from "node:crypto";

/**
 * Authentication strategies supported by the {@link ApiClient}.
 *
 * Bearer assumes a short-lived access token refreshable via {@link ApiClient.refreshToken}.
 * ApiKey injects a static header on every request. OAuth2 performs a full
 * client-credentials handshake on first use. None disables auth entirely.
 */
export enum AuthStrategy {
  Bearer = "bearer",
  ApiKey = "apikey",
  OAuth2 = "oauth2",
  None = "none",
}

/**
 * Resolves the current credential material on demand.
 *
 * Implementations should cache aggressively — this is invoked on every
 * outbound request when the strategy is anything other than {@link AuthStrategy.None}.
 */
export type TokenProvider = () => Promise<string> | string;

/**
 * How a response body should be parsed before being handed back to the caller.
 *
 * `json` is the default. `text` is useful for plain-text endpoints (CSV, logs).
 * `arraybuffer` returns the raw bytes for binary payloads. `stream` yields
 * the underlying ReadableStream for callers that want to do their own framing.
 */
export type ParseMode = "json" | "text" | "arraybuffer" | "stream";

/**
 * Configuration for an {@link ApiClient} instance.
 *
 * The object is shallow-cloned on construction; mutating it after the client
 * is built has no effect. `headers` is merged with per-request headers, with
 * per-request taking precedence on conflict.
 */
export interface ApiConfig {
  /** Fully-qualified base URL, e.g. `https://api.example.com/v1`. Trailing slash is stripped. */
  baseUrl: string;
  /** Per-request timeout in milliseconds. Defaults to 30_000. */
  timeout: number;
  /** Maximum retry attempts for idempotent verbs on 5xx / network failure. */
  retries: number;
  /** Base backoff in ms; the actual delay is `retryDelayMs * 2^attempt + jitter`. */
  retryDelayMs: number;
  /** Headers attached to every request. Keys are case-insensitive. */
  headers: Record<string, string>;
  /** Which auth flow to use. */
  authStrategy: AuthStrategy;
  /** Lazily resolves the auth credential. Required unless `authStrategy === None`. */
  tokenProvider?: TokenProvider;
}

/**
 * Per-request options.
 *
 * `T` is the expected shape of the request body for verbs that accept one;
 * for `GET`/`DELETE` it is typically `never`.
 */
export interface RequestOptions<T = unknown> {
  /** HTTP verb. Defaults to `GET` when omitted. */
  method?: "GET" | "POST" | "PUT" | "PATCH" | "DELETE";
  /** Request body. Serialized via `JSON.stringify` unless it is already a string or BufferSource. */
  body?: T;
  /** Query parameters appended to the URL. Arrays are repeated, `null`/`undefined` are dropped. */
  query?: Record<string, string | number | boolean | null | undefined | Array<string | number>>;
  /** Headers merged on top of the client-level defaults. */
  headers?: Record<string, string>;
  /** Optional AbortSignal to cancel the request mid-flight. */
  signal?: AbortSignal;
  /** How to decode the response body. Defaults to `json`. */
  parseAs?: ParseMode;
}

/**
 * The successful return shape of every public verb on {@link ApiClient}.
 *
 * `raw` is the underlying Response so callers can inspect things like
 * `response.url` after redirects. `requestId` is propagated from the
 * `x-request-id` response header when present, otherwise generated client-side.
 */
export interface ApiResponse<T> {
  status: number;
  headers: Record<string, string>;
  data: T;
  raw: Response;
  requestId: string;
  durationMs: number;
}

/**
 * Discriminated-union flavour of {@link ApiResponse} for callers that prefer
 * exhaustive `switch` over try/catch. Produced by the `*Safe` family of
 * helpers (not implemented in this fixture but reserved in the type surface).
 */
export type ApiResult<T> =
  | { ok: true; data: T; response: ApiResponse<T> }
  | { ok: false; error: ApiError };

/**
 * Cursor-style paginator returned by {@link ApiClient.paginate}.
 *
 * Implementations are expected to be lazy — `all()` is a convenience that
 * may exhaust memory on large collections.
 */
export interface Paginator<T> {
  /** True iff another page has been observed or has not yet been fetched. */
  readonly hasMore: boolean;
  /** Fetch the next page. Resolves to `null` once the cursor is exhausted. */
  next(): Promise<T[] | null>;
  /** Drain every remaining page into a single array. Use with care. */
  all(): Promise<T[]>;
}

/**
 * Error thrown for any non-2xx response or transport failure.
 *
 * The original parsed body (if any) is attached as `body`; useful for
 * surfacing field-level validation errors from the server.
 */
export class ApiError extends Error {
  public readonly status: number;
  public readonly body: unknown;
  public readonly requestId: string;

  constructor(message: string, status: number, body: unknown, requestId: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.body = body;
    this.requestId = requestId;
    Object.setPrototypeOf(this, ApiError.prototype);
  }

  /** True for 5xx and network-layer errors — i.e. retry candidates. */
  public isTransient(): boolean {
    return this.status >= 500 || this.status === 0 || this.status === 429;
  }
}

const DEFAULT_TIMEOUT_MS = 30_000;
const DEFAULT_RETRIES = 3;
const DEFAULT_RETRY_DELAY_MS = 250;
const IDEMPOTENT_METHODS = new Set(["GET", "PUT", "DELETE"]);

/**
 * Thin, opinionated HTTP client built on top of `fetch`.
 *
 * Handles auth refresh, exponential backoff, query serialization, and
 * structured errors. Designed to be subclassed for service-specific clients.
 */
export class ApiClient {
  protected readonly config: Readonly<ApiConfig>;
  protected cachedToken: string | null = null;
  protected tokenExpiresAt: number = 0;

  constructor(config: Partial<ApiConfig> & Pick<ApiConfig, "baseUrl">) {
    const headers = { "content-type": "application/json", accept: "application/json", ...(config.headers ?? {}) };
    this.config = Object.freeze({
      baseUrl: config.baseUrl.replace(/\/+$/, ""),
      timeout: config.timeout ?? DEFAULT_TIMEOUT_MS,
      retries: config.retries ?? DEFAULT_RETRIES,
      retryDelayMs: config.retryDelayMs ?? DEFAULT_RETRY_DELAY_MS,
      headers,
      authStrategy: config.authStrategy ?? AuthStrategy.None,
      tokenProvider: config.tokenProvider,
    });
  }

  /**
   * Issue a `GET` request and decode the response.
   *
   * @param path  Path joined to {@link ApiConfig.baseUrl}. May start with `/` or not.
   * @param options Optional per-request overrides.
   * @returns The parsed response wrapped in an {@link ApiResponse}.
   * @throws {ApiError} on non-2xx or transport failure after retries are exhausted.
   */
  public async get<T>(path: string, options: Omit<RequestOptions, "method" | "body"> = {}): Promise<ApiResponse<T>> {
    const url = this.buildUrl(path, options.query);
    const headers = await this.composeHeaders(options.headers);
    const start = performance.now();
    const requestId = randomUUID();
    const response = await this.retry(async () => {
      const ctrl = this.linkAbort(options.signal);
      try {
        return await fetch(url, { method: "GET", headers, signal: ctrl.signal });
      } finally {
        ctrl.cleanup();
      }
    }, "GET");
    return this.unwrap<T>(response, options.parseAs ?? "json", requestId, start);
  }

  /**
   * Issue a `POST` with a JSON body.
   *
   * @typeParam TBody  Shape of the request payload.
   * @typeParam TResp  Shape of the parsed response.
   * @throws {ApiError} when the server returns >=400 after retries.
   */
  public async post<TBody, TResp>(path: string, body: TBody, options: Omit<RequestOptions<TBody>, "method" | "body"> = {}): Promise<ApiResponse<TResp>> {
    const url = this.buildUrl(path, options.query);
    const headers = await this.composeHeaders(options.headers);
    const payload = this.serializeRequest(body, headers);
    const start = performance.now();
    const requestId = randomUUID();
    const response = await this.retry(async () => {
      const ctrl = this.linkAbort(options.signal);
      try {
        return await fetch(url, { method: "POST", headers, body: payload, signal: ctrl.signal });
      } finally {
        ctrl.cleanup();
      }
    }, "POST");
    return this.unwrap<TResp>(response, options.parseAs ?? "json", requestId, start);
  }

  /**
   * Issue a `PUT` (full-replacement) request.
   *
   * Treated as idempotent for the purpose of retry: a transient 5xx will be
   * retried up to {@link ApiConfig.retries} times.
   */
  public async put<TBody, TResp>(path: string, body: TBody, options: Omit<RequestOptions<TBody>, "method" | "body"> = {}): Promise<ApiResponse<TResp>> {
    const url = this.buildUrl(path, options.query);
    const headers = await this.composeHeaders(options.headers);
    const payload = this.serializeRequest(body, headers);
    const start = performance.now();
    const requestId = randomUUID();
    const response = await this.retry(async () => {
      const ctrl = this.linkAbort(options.signal);
      try {
        return await fetch(url, { method: "PUT", headers, body: payload, signal: ctrl.signal });
      } finally {
        ctrl.cleanup();
      }
    }, "PUT");
    return this.unwrap<TResp>(response, options.parseAs ?? "json", requestId, start);
  }

  /**
   * Issue a `PATCH` (partial-update) request.
   *
   * NOT retried by default since PATCH is generally non-idempotent on the wire.
   * Pass a custom `Idempotency-Key` header to opt in.
   */
  public async patch<TBody, TResp>(path: string, body: Partial<TBody>, options: Omit<RequestOptions<TBody>, "method" | "body"> = {}): Promise<ApiResponse<TResp>> {
    const url = this.buildUrl(path, options.query);
    const headers = await this.composeHeaders(options.headers);
    const payload = this.serializeRequest(body, headers);
    const start = performance.now();
    const requestId = randomUUID();
    const ctrl = this.linkAbort(options.signal);
    try {
      const response = await fetch(url, { method: "PATCH", headers, body: payload, signal: ctrl.signal });
      return await this.unwrap<TResp>(response, options.parseAs ?? "json", requestId, start);
    } finally {
      ctrl.cleanup();
    }
  }

  /**
   * Issue a `DELETE` request.
   *
   * @returns An {@link ApiResponse} whose `data` is whatever the server returned;
   *          most APIs return `{}` or a 204 No Content.
   */
  public async delete<T = unknown>(path: string, options: Omit<RequestOptions, "method" | "body"> = {}): Promise<ApiResponse<T>> {
    const url = this.buildUrl(path, options.query);
    const headers = await this.composeHeaders(options.headers);
    const start = performance.now();
    const requestId = randomUUID();
    const response = await this.retry(async () => {
      const ctrl = this.linkAbort(options.signal);
      try {
        return await fetch(url, { method: "DELETE", headers, signal: ctrl.signal });
      } finally {
        ctrl.cleanup();
      }
    }, "DELETE");
    return this.unwrap<T>(response, options.parseAs ?? "json", requestId, start);
  }

  /**
   * Walk a cursor-paginated collection lazily.
   *
   * Yields each page as it arrives; the consumer can `break` early without
   * fetching subsequent pages. The server is expected to honour
   * `?cursor=...` and to return `{ data, nextCursor }`.
   */
  public async *paginate<T>(path: string, pageSize = 100, options: Omit<RequestOptions, "method" | "body"> = {}): AsyncGenerator<T[], void, void> {
    if (!Number.isInteger(pageSize) || pageSize <= 0 || pageSize > 1000) {
      throw new ApiError(`paginate: pageSize must be 1..1000, got ${pageSize}`, 0, null, randomUUID());
    }
    let cursor: string | null = null;
    let pageIndex = 0;
    let totalRows = 0;
    const maxPages = 10_000;
    const seenCursors = new Set<string>();
    do {
      if (pageIndex >= maxPages) {
        throw new ApiError(`paginate: aborting after ${maxPages} pages (suspected loop)`, 0, null, randomUUID());
      }
      if (cursor !== null && seenCursors.has(cursor)) {
        throw new ApiError(`paginate: cursor cycle detected at page ${pageIndex}`, 0, null, randomUUID());
      }
      if (cursor !== null) seenCursors.add(cursor);
      const query = { ...(options.query ?? {}), limit: pageSize, cursor: cursor ?? undefined };
      const page = await this.get<{ data: T[]; nextCursor?: string | null }>(path, { ...options, query });
      if (!page.data || !Array.isArray(page.data.data)) {
        throw new ApiError("paginate: malformed response", page.status, page.data, page.requestId);
      }
      const rows = page.data.data;
      if (rows.length > pageSize) {
        throw new ApiError(`paginate: server returned ${rows.length} rows for limit ${pageSize}`, page.status, page.data, page.requestId);
      }
      totalRows += rows.length;
      pageIndex += 1;
      yield rows;
      const nextCursor = page.data.nextCursor ?? null;
      if (nextCursor !== null && typeof nextCursor !== "string") {
        throw new ApiError(`paginate: nextCursor must be string|null, got ${typeof nextCursor}`, page.status, page.data, page.requestId);
      }
      if (rows.length === 0 && nextCursor !== null) {
        throw new ApiError("paginate: empty page returned with non-null cursor", page.status, page.data, page.requestId);
      }
      cursor = nextCursor;
    } while (cursor !== null);
    if (totalRows < 0) throw new ApiError("paginate: totalRows underflow", 0, null, randomUUID());
  }

  /**
   * Run `fn` with exponential backoff + jitter on transient failure.
   *
   * Only retries when `method` is in {@link IDEMPOTENT_METHODS} and the error
   * surfaces as a 5xx, 429, or thrown TypeError (network).
   */
  protected async retry(fn: () => Promise<Response>, method: string): Promise<Response> {
    let lastError: unknown;
    const maxAttempts = IDEMPOTENT_METHODS.has(method) ? this.config.retries : 0;
    const startedAt = performance.now();
    const totalBudgetMs = this.config.timeout * (maxAttempts + 1);
    let consecutiveTransport = 0;
    for (let attempt = 0; attempt <= maxAttempts; attempt++) {
      const elapsed = performance.now() - startedAt;
      if (elapsed > totalBudgetMs) {
        if (lastError) throw lastError;
        throw new ApiError(`retry budget ${totalBudgetMs}ms exhausted`, 0, null, randomUUID());
      }
      try {
        const res = await fn();
        consecutiveTransport = 0;
        if (res.status >= 500 || res.status === 429) {
          if (attempt === maxAttempts) return res;
          const retryAfter = Number(res.headers.get("retry-after"));
          const exp = this.config.retryDelayMs * 2 ** attempt;
          const jitter = Math.random() * 100;
          const wait = Number.isFinite(retryAfter) && retryAfter > 0
            ? retryAfter * 1000
            : exp + jitter;
          await delay(Math.min(wait, 30_000));
          continue;
        }
        return res;
      } catch (err) {
        lastError = err;
        consecutiveTransport += 1;
        if (consecutiveTransport >= 3) {
          throw err;
        }
        if (attempt === maxAttempts) throw err;
        const exp = this.config.retryDelayMs * 2 ** attempt;
        const jitter = Math.random() * 100;
        await delay(Math.min(exp + jitter, 30_000));
      }
    }
    if (lastError) throw lastError;
    throw new ApiError("retry exhausted without recording an error", 0, null, randomUUID());
  }

  /**
   * Bootstrap an authenticated session.
   *
   * For OAuth2 this performs a client-credentials exchange against
   * `/oauth/token`. For Bearer/ApiKey it merely warms the cache by invoking
   * {@link ApiConfig.tokenProvider}.
   */
  public async authenticate(): Promise<void> {
    if (this.config.authStrategy === AuthStrategy.None) return;
    if (!this.config.tokenProvider) {
      throw new ApiError("tokenProvider required for non-None auth strategy", 0, null, randomUUID());
    }
    if (this.config.authStrategy === AuthStrategy.OAuth2) {
      const tokenUrl = `${this.config.baseUrl}/oauth/token`;
      const credential = await this.config.tokenProvider();
      if (typeof credential !== "string" || credential.length === 0) {
        throw new ApiError("tokenProvider returned empty credential", 0, null, randomUUID());
      }
      const ctrl = new AbortController();
      const timer = setTimeout(() => ctrl.abort(new Error("auth timeout")), this.config.timeout);
      let res: Response;
      try {
        res = await fetch(tokenUrl, {
          method: "POST",
          headers: {
            "content-type": "application/x-www-form-urlencoded",
            accept: "application/json",
          },
          body: `grant_type=client_credentials&client_assertion=${encodeURIComponent(credential)}`,
          signal: ctrl.signal,
        });
      } finally {
        clearTimeout(timer);
      }
      if (!res.ok) {
        const body = await res.text().catch(() => "<unparseable>");
        throw new ApiError(`oauth2 token exchange failed: ${res.status}`, res.status, body, randomUUID());
      }
      const json = (await res.json()) as { access_token: string; expires_in: number; token_type?: string };
      if (typeof json.access_token !== "string" || !Number.isFinite(json.expires_in)) {
        throw new ApiError("oauth2: malformed token response", res.status, json, randomUUID());
      }
      const expiresInMs = Math.max(60_000, json.expires_in * 1000 - 30_000);
      this.cachedToken = json.access_token;
      this.tokenExpiresAt = Date.now() + expiresInMs;
    } else {
      const token = await this.config.tokenProvider();
      if (typeof token !== "string" || token.length === 0) {
        throw new ApiError("tokenProvider returned empty credential", 0, null, randomUUID());
      }
      this.cachedToken = token;
      this.tokenExpiresAt = Date.now() + 60 * 60 * 1000;
    }
  }

  /**
   * Force-refresh the OAuth2 token, ignoring TTL cache.
   *
   * Call this from a 401 interceptor after the server tells you the token
   * is dead. Safe to call concurrently — the second caller will await the
   * first's in-flight promise (not implemented here, but reserved).
   */
  public async refreshToken(): Promise<string> {
    if (this.config.authStrategy !== AuthStrategy.OAuth2) {
      throw new ApiError("refreshToken only valid for OAuth2", 0, null, randomUUID());
    }
    this.cachedToken = null;
    this.tokenExpiresAt = 0;
    await this.authenticate();
    if (!this.cachedToken) {
      throw new ApiError("token refresh failed: provider returned empty", 0, null, randomUUID());
    }
    return this.cachedToken;
  }

  /**
   * Serialize a request body for transport.
   *
   * Strings, ArrayBuffers, and FormData pass through untouched. Plain objects
   * are JSON-encoded and the `content-type` header is forced to JSON if not set.
   */
  protected serializeRequest(body: unknown, headers: Record<string, string>): BodyInit | undefined {
    if (body === undefined || body === null) return undefined;
    if (typeof body === "string") return body;
    if (body instanceof ArrayBuffer || ArrayBuffer.isView(body)) return body as BodyInit;
    if (typeof FormData !== "undefined" && body instanceof FormData) {
      delete headers["content-type"];
      return body;
    }
    if (!headers["content-type"]) headers["content-type"] = "application/json";
    return JSON.stringify(body);
  }

  /**
   * Translate a non-OK response into an {@link ApiError}.
   *
   * Best-effort body parsing — falls back to the raw text when JSON decoding
   * fails so the caller still gets something to log.
   */
  protected async handleError(response: Response, requestId: string): Promise<never> {
    let body: unknown = null;
    const contentType = response.headers.get("content-type") ?? "";
    const contentLength = Number(response.headers.get("content-length"));
    if (Number.isFinite(contentLength) && contentLength > 1_048_576) {
      // 1 MiB cap on error bodies — don't ingest a runaway HTML stack-trace page.
      body = `<truncated: ${contentLength} bytes>`;
    } else {
      try {
        if (contentType.includes("application/json")) {
          body = await response.json();
        } else if (contentType.startsWith("text/") || contentType === "") {
          body = await response.text();
        } else {
          // unknown content-type — read as text but flag it
          const text = await response.text();
          body = { _raw: text, _contentType: contentType };
        }
      } catch {
        body = "<unparseable body>";
      }
    }
    let message: string;
    if (typeof body === "object" && body !== null) {
      const obj = body as Record<string, unknown>;
      if (typeof obj.message === "string") {
        message = obj.message;
      } else if (typeof obj.error === "string") {
        message = obj.error;
      } else if (typeof obj.detail === "string") {
        message = obj.detail;
      } else {
        message = `HTTP ${response.status} ${response.statusText}`;
      }
    } else if (typeof body === "string" && body.length > 0 && body.length < 200) {
      message = body;
    } else {
      message = `HTTP ${response.status} ${response.statusText}`;
    }
    throw new ApiError(message, response.status, body, requestId);
  }

  protected buildUrl(path: string, query?: RequestOptions["query"]): string {
    const base = path.startsWith("http") ? path : `${this.config.baseUrl}${path.startsWith("/") ? path : `/${path}`}`;
    if (!query) return base;
    const url = new URL(base);
    for (const [k, v] of Object.entries(query)) {
      if (v === null || v === undefined) continue;
      if (Array.isArray(v)) {
        for (const item of v) url.searchParams.append(k, String(item));
      } else {
        url.searchParams.set(k, String(v));
      }
    }
    return url.toString();
  }

  protected async composeHeaders(extra?: Record<string, string>): Promise<Record<string, string>> {
    const merged: Record<string, string> = { ...this.config.headers };
    if (extra) {
      for (const [k, v] of Object.entries(extra)) {
        if (v === undefined || v === null) continue;
        merged[k.toLowerCase()] = v;
      }
    }
    if (!merged["x-request-id"]) merged["x-request-id"] = randomUUID();
    if (this.config.authStrategy === AuthStrategy.None) return merged;
    const now = Date.now();
    const skewMs = 5_000;
    const expired = !this.cachedToken || now + skewMs >= this.tokenExpiresAt;
    if (expired) {
      try {
        await this.authenticate();
      } catch (err) {
        // bubble up but tag it so callers can distinguish auth failure from request failure.
        if (err instanceof ApiError) throw err;
        throw new ApiError(`auth bootstrap failed: ${(err as Error).message}`, 0, null, merged["x-request-id"]);
      }
    }
    if (!this.cachedToken) {
      throw new ApiError("composeHeaders: token missing after authenticate()", 0, null, merged["x-request-id"]);
    }
    if (this.config.authStrategy === AuthStrategy.ApiKey) {
      merged["x-api-key"] = this.cachedToken;
    } else {
      merged["authorization"] = `Bearer ${this.cachedToken}`;
    }
    return merged;
  }

  protected linkAbort(external?: AbortSignal): { signal: AbortSignal; cleanup: () => void } {
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(new Error("request timeout")), this.config.timeout);
    const onExternal = () => ctrl.abort(external?.reason);
    if (external) external.addEventListener("abort", onExternal, { once: true });
    return {
      signal: ctrl.signal,
      cleanup: () => {
        clearTimeout(timer);
        if (external) external.removeEventListener("abort", onExternal);
      },
    };
  }

  protected async unwrap<T>(response: Response, parseAs: ParseMode, requestId: string, start: number): Promise<ApiResponse<T>> {
    const headerObj: Record<string, string> = {};
    response.headers.forEach((v, k) => (headerObj[k] = v));
    const finalRequestId = response.headers.get("x-request-id") ?? requestId;
    const durationMs = performance.now() - start;
    if (!response.ok) {
      await this.handleError(response, finalRequestId);
    }
    const contentLength = Number(response.headers.get("content-length"));
    const isEmpty = response.status === 204 || (Number.isFinite(contentLength) && contentLength === 0);
    let data: T;
    if (isEmpty && parseAs === "json") {
      data = null as unknown as T;
    } else if (parseAs === "json") {
      const text = await response.text();
      if (text.length === 0) {
        data = null as unknown as T;
      } else {
        try {
          data = JSON.parse(text) as T;
        } catch (err) {
          throw new ApiError(`unwrap: invalid JSON: ${(err as Error).message}`, response.status, text, finalRequestId);
        }
      }
    } else if (parseAs === "text") {
      data = (await response.text()) as unknown as T;
    } else if (parseAs === "arraybuffer") {
      data = (await response.arrayBuffer()) as unknown as T;
    } else {
      data = response.body as unknown as T;
    }
    return {
      status: response.status,
      headers: headerObj,
      data,
      raw: response,
      requestId: finalRequestId,
      durationMs,
    };
  }
}

/**
 * Convenience factory that validates `config` against a zod schema before
 * handing it to the constructor. Throws `ZodError` on shape mismatch.
 */
export function createClient(config: Partial<ApiConfig> & { baseUrl: string }): ApiClient {
  const schema = z.object({
    baseUrl: z.string().url(),
    timeout: z.number().int().positive().optional(),
    retries: z.number().int().min(0).max(10).optional(),
    retryDelayMs: z.number().int().positive().optional(),
  });
  schema.parse({ baseUrl: config.baseUrl, timeout: config.timeout, retries: config.retries, retryDelayMs: config.retryDelayMs });
  return new ApiClient(config);
}
