'use strict';

const test = require('node:test');
const assert = require('node:assert');
const { Database } = require('../index.js');

function available(name) {
  try {
    require.resolve(name);
    return true;
  } catch {
    return false;
  }
}

test('Drizzle runs through the SQLite proxy', { skip: !available('drizzle-orm/sqlite-proxy') }, async () => {
  const { drizzle } = require('../drizzle.js');
  const { sqliteTable, integer, text } = require('drizzle-orm/sqlite-core');
  const database = new Database(':memory:');
  database.execute('CREATE TABLE eggs (id INTEGER PRIMARY KEY, colour TEXT NOT NULL)');
  const eggs = sqliteTable('eggs', { id: integer('id').primaryKey(), colour: text('colour').notNull() });
  const db = drizzle(database);
  await db.insert(eggs).values([{ colour: 'brown' }, { colour: 'white' }]);
  const rows = await db.select().from(eggs).orderBy(eggs.id);
  assert.deepStrictEqual(rows.map((row) => row.colour), ['brown', 'white']);
  database.close();
});

test('Knex builds, runs and counts through the EggDB client', { skip: !available('knex') }, async () => {
  const knex = require('knex');
  const ClientEggDb = require('../knex.js');
  const db = knex({ client: ClientEggDb, connection: { filename: ':memory:' }, useNullAsDefault: true, pool: { min: 1, max: 1 } });
  await db.schema.createTable('eggs', (table) => {
    table.increments('id');
    table.string('colour').notNullable();
  });
  const [id] = await db('eggs').insert({ colour: 'brown' });
  await db('eggs').insert({ colour: 'white' });
  assert.strictEqual(Number(id), 1);
  const changed = await db('eggs').where({ colour: 'white' }).update({ colour: 'speckled' });
  assert.strictEqual(changed, 1);
  const colours = await db('eggs').orderBy('id').pluck('colour');
  assert.deepStrictEqual(colours, ['brown', 'speckled']);
  await db.destroy();
});
