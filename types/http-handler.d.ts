// Generated from src/waffle_backend/http/handler/world.wit by the SDK.
declare module "perry:http-handler/types" {
/** Outbound byte lists accept UTF-8 text; inbound values remain Uint8Array. */
const __witResourceBrand: unique symbol;
export interface WitResource { readonly [__witResourceBrand]: true; }
export type WitInput<T> = T extends WitResource ? T : T extends Uint8Array ? string | Uint8Array : T extends object ? { [K in keyof T]: WitInput<T[K]> } : T;

export type Method =
  | { tag: "get" }
  | { tag: "head" }
  | { tag: "post" }
  | { tag: "put" }
  | { tag: "delete" }
  | { tag: "connect" }
  | { tag: "options" }
  | { tag: "trace" }
  | { tag: "patch" }
  | { tag: "other"; val: string };

export type Scheme =
  | { tag: "http" }
  | { tag: "https" }
  | { tag: "other"; val: string };

export type RequestInput = WitInput<Request>;
export interface Request {
  method: Method;
  scheme: Scheme | null | undefined;
  authority: string | null | undefined;
  pathWithQuery: string | null | undefined;
  headers: Array<[string, Uint8Array]>;
  body: Uint8Array;
}

export type ResponseInput = WitInput<Response>;
export interface Response {
  status: number;
  headers: Array<[string, Uint8Array]>;
  body: Uint8Array;
}


}
