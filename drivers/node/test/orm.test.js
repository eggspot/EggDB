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

test('the sqlite3-compatible module rewrites placeholders and reports changes', async () => {
  const sqlite3 = require('../sqlite3.js');
  assert.deepStrictEqual(sqlite3.positional('SELECT $2, ?1, :name', [10, 20]).values, [20, 10, undefined]);
  assert.deepStrictEqual(sqlite3.positional("SELECT '?', $a", { $a: 1 }), { sql: "SELECT '?', ?", values: [1] });
  const db = await new Promise((resolve, reject) => {
    const opened = new sqlite3.Database(':memory:', (error) => (error ? reject(error) : resolve(opened)));
  });
  const run = (sql, parameters) =>
    new Promise((resolve, reject) => db.run(sql, parameters, function done(error) {
      return error ? reject(error) : resolve({ lastID: this.lastID, changes: this.changes });
    }));
  await run('CREATE TABLE eggs (id INTEGER PRIMARY KEY, colour TEXT)');
  assert.deepStrictEqual(await run('INSERT INTO eggs (colour) VALUES (?)', ['brown']), { lastID: 1, changes: 1 });
  const rows = await new Promise((resolve, reject) =>
    db.all('SELECT colour FROM eggs WHERE id = $id', { $id: 1 }, (error, found) => (error ? reject(error) : resolve(found))));
  assert.deepStrictEqual(rows, [{ colour: 'brown' }]);
  await new Promise((resolve) => db.close(resolve));
});

test('Sequelize runs on its SQLite dialect over EggDB', { skip: !available('sequelize') }, async () => {
  const { Sequelize, DataTypes } = require('sequelize');
  const sequelize = new Sequelize({ dialect: 'sqlite', dialectModule: require('../sqlite3.js'), storage: ':memory:', logging: false });
  const Egg = sequelize.define('Egg', { colour: { type: DataTypes.STRING, allowNull: false } }, { timestamps: false });
  await sequelize.sync();
  await Egg.create({ colour: 'brown' });
  await Egg.create({ colour: 'white' });
  const found = await Egg.findAll({ order: [['id', 'ASC']] });
  assert.deepStrictEqual(found.map((egg) => egg.colour), ['brown', 'white']);
  await sequelize.close();
});

test('TypeORM runs on its SQLite driver over EggDB', { skip: !available('typeorm') }, async () => {
  const { DataSource, EntitySchema } = require('typeorm');
  const Egg = new EntitySchema({
    name: 'Egg',
    tableName: 'eggs',
    columns: { id: { type: Number, primary: true, generated: true }, colour: { type: String } },
  });
  const source = new DataSource({ type: 'sqlite', database: ':memory:', driver: require('../sqlite3.js'), entities: [Egg], synchronize: true });
  await source.initialize();
  const eggs = source.getRepository('Egg');
  await eggs.save({ colour: 'brown' });
  const found = await eggs.find();
  assert.deepStrictEqual(found.map((egg) => egg.colour), ['brown']);
  await source.destroy();
});
