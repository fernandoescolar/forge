// SQL text for scripts the explorer writes for you (SELECT … to start a query).
import type { Engine } from './client';

export function quote(engine: Engine, name: string): string {
  switch (engine) {
    case 'mysql':
    case 'mariadb':
      return '`' + name.replace(/`/g, '``') + '`';
    case 'mssql':
      return '[' + name.replace(/]/g, ']]') + ']';
    default:
      return '"' + name.replace(/"/g, '""') + '"';
  }
}

export function qualified(engine: Engine, schema: string | null | undefined, table: string): string {
  return schema ? `${quote(engine, schema)}.${quote(engine, table)}` : quote(engine, table);
}

export function selectScript(engine: Engine, schema: string | null | undefined, table: string, limit = 100): string {
  const name = qualified(engine, schema, table);
  return engine === 'mssql' ? `SELECT TOP ${limit} *\nFROM ${name};` : `SELECT *\nFROM ${name}\nLIMIT ${limit};`;
}
