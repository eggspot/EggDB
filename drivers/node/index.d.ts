export type Dialect = 'sqlite' | 'postgresql' | 'mysql' | 'oracle' | 'sqlserver';

export type Value = null | number | bigint | string | Buffer;

export type Parameter = Value | boolean | Uint8Array | Date | undefined;

export interface OpenOptions {
  dialect?: Dialect;
  readOnly?: boolean;
}

export interface StatementResult {
  tag: string;
  columns: string[];
  rows: Value[][];
}

export class EggDbError extends Error {
  sqlstate: string | null;
  status: number | undefined;
}

export class Database {
  constructor(file: string, options?: OpenOptions);
  readonly dialect: Dialect;
  readonly readOnly: boolean;
  execute(sql: string, parameters?: Parameter[]): StatementResult[];
  query<Row = Record<string, Value>>(sql: string, parameters?: Parameter[]): Row[];
  run(sql: string, parameters?: Parameter[]): { changes: number };
  transaction<T>(body: (database: Database) => T): T;
  close(): void;
}

export type Bytes = string | Buffer | Uint8Array;

export interface ScanOptions {
  start?: Bytes | null;
  end?: Bytes | null;
  prefix?: Bytes | null;
  reverse?: boolean;
  endInclusive?: boolean;
  collection?: string | null;
}

export interface CollectionStats {
  entries: bigint;
  keyBytes: bigint;
  valueBytes: bigint;
}

export class Transaction {
  readonly writing: boolean;
  get(key: Bytes, collection?: string | null): Buffer | null;
  put(key: Bytes, value: Bytes, collection?: string | null, ttlMs?: number | null): void;
  delete(key: Bytes, collection?: string | null): boolean;
  scan(options?: ScanOptions): [Buffer, Buffer][];
  createCollection(name: string): boolean;
  dropCollection(name: string): boolean;
  renameCollection(name: string, newName: string): void;
  collectionStats(name: string): CollectionStats;
  collections(): string[];
  commit(): bigint;
  rollback(): void;
}

export class KeyValue {
  constructor(file: string, options?: { readOnly?: boolean });
  readonly readOnly: boolean;
  read<T>(body: (transaction: Transaction) => T): T;
  write<T>(body: (transaction: Transaction) => T): T;
  close(): void;
}
