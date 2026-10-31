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
