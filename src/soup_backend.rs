use distributary::{ControllerHandle, DataType, Mutator, RemoteGetter, ZookeeperAuthority};

use msql_srv::{self, *};
use nom_sql::{self, ColumnConstraint, ConditionBase, ConditionExpression, ConditionTree,
              CreateTableStatement, FieldExpression, Operator, SelectStatement};

use slog;
use std::io;
use std::collections::{BTreeMap, HashMap};
use std::sync::{self, Arc, Mutex};

use utils;
use schema::schema_for_query;

pub struct SoupBackend {
    soup: ControllerHandle<ZookeeperAuthority>,
    log: slog::Logger,

    inputs: BTreeMap<String, Mutator>,
    outputs: BTreeMap<String, RemoteGetter>,

    table_schemas: Arc<Mutex<HashMap<String, CreateTableStatement>>>,
    auto_increments: Arc<Mutex<HashMap<String, u64>>>,

    query_count: Arc<sync::atomic::AtomicUsize>,

    prepared: HashMap<String, u32>,
    prepared_count: u32,
}

impl SoupBackend {
    pub fn new(
        zk_addr: &str,
        deployment_id: &str,
        schemas: Arc<Mutex<HashMap<String, CreateTableStatement>>>,
        auto_increments: Arc<Mutex<HashMap<String, u64>>>,
        query_counter: Arc<sync::atomic::AtomicUsize>,
        log: slog::Logger,
    ) -> Self {
        let mut zk_auth = ZookeeperAuthority::new(&format!("{}/{}", zk_addr, deployment_id));
        zk_auth.log_with(log.clone());

        debug!(log, "Connecting to Soup...",);
        let mut ch = ControllerHandle::new(zk_auth);

        let inputs = ch.inputs()
            .into_iter()
            .map(|(n, _)| (n.clone(), ch.get_mutator(&n).unwrap()))
            .collect::<BTreeMap<String, Mutator>>();
        let outputs = ch.outputs()
            .into_iter()
            .map(|(n, _)| (n.clone(), ch.get_getter(&n).unwrap()))
            .collect::<BTreeMap<String, RemoteGetter>>();

        debug!(log, "Connected!");

        SoupBackend {
            soup: ch,
            log: log,

            inputs: inputs,
            outputs: outputs,

            table_schemas: schemas,
            auto_increments: auto_increments,

            query_count: query_counter,

            prepared: HashMap::new(),
            prepared_count: 0,
        }
    }

    fn handle_create_table<W: io::Write>(
        &mut self,
        q: nom_sql::CreateTableStatement,
        results: QueryResultWriter<W>,
    ) -> io::Result<()> {
        match self.soup.extend_recipe(format!("{};", q)) {
            Ok(_) => {
                let mut ts_lock = self.table_schemas.lock().unwrap();
                ts_lock.insert(q.table.name.clone(), q);
                // no rows to return
                // TODO(malte): potentially eagerly cache the mutator for this table
                results.completed(0, 0)
            }
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e)),
        }
    }

    fn handle_delete<W: io::Write>(
        &mut self,
        q: nom_sql::DeleteStatement,
        results: QueryResultWriter<W>,
    ) -> io::Result<()> {
        let cond = q.where_clause
            .expect("only supports DELETEs with WHERE-clauses");

        let ts = self.table_schemas.lock().unwrap();
        let pkey = utils::get_primary_key(&ts[&q.table.name])
            .into_iter()
            .map(|(_, c)| c)
            .collect();

        // create a mutator if we don't have one for this table already
        let mutator = self.inputs
            .entry(q.table.name.clone())
            .or_insert(self.soup.get_mutator(&q.table.name).unwrap());

        match utils::flatten_conditional(&cond, &pkey) {
            None => results.completed(0, 0),
            Some(ref flattened) if flattened.len() == 0 => {
                panic!("DELETE only supports WHERE-clauses on primary keys");
            }
            Some(flattened) => {
                let count = flattened.len() as u64;
                for key in flattened {
                    match mutator.delete(key) {
                        Ok(_) => {}
                        Err(e) => {
                            error!(self.log, "delete error: {:?}", e);
                            return results.error(
                                msql_srv::ErrorKind::ER_UNKNOWN_ERROR,
                                format!("{:?}", e).as_bytes(),
                            );
                        }
                    };
                }

                results.completed(count, 0)
            }
        }
    }

    fn handle_insert<W: io::Write>(
        &mut self,
        q: nom_sql::InsertStatement,
        results: QueryResultWriter<W>,
    ) -> io::Result<()> {
        let table = q.table.name.clone();

        // create a mutator if we don't have one for this table already
        let putter = self.inputs
            .entry(table.clone())
            .or_insert(self.soup.get_mutator(&table).unwrap());

        let schema: Vec<String> = putter.columns().to_vec();
        let mut data: Vec<Vec<DataType>> =
            vec![vec![DataType::from(0 as i32); schema.len()]; q.data.len()];

        let ts_lock = self.table_schemas.lock().unwrap();
        let auto_increment_columns: Vec<_> = ts_lock[&table]
            .fields
            .iter()
            .filter(|c| c.constraints.contains(&ColumnConstraint::AutoIncrement))
            .collect();

        // can only have zero or one AUTO_INCREMENT columns
        assert!(auto_increment_columns.len() <= 1);

        let mut ai_lock = self.auto_increments.lock().unwrap();
        let auto_increment: &mut u64 = &mut (*ai_lock).entry(table.clone()).or_insert(0);
        let last_insert_id = *auto_increment + 1;

        for (ri, ref row) in q.data.iter().enumerate() {
            if let Some(col) = auto_increment_columns.iter().next() {
                let idx = schema
                    .iter()
                    .position(|f| *f == col.column.name)
                    .expect(&format!("no column named '{}'", col.column.name));
                *auto_increment += 1;
                data[ri][idx] = DataType::from(*auto_increment as i64);
            }

            for (ci, c) in q.fields.iter().enumerate() {
                let idx = schema
                    .iter()
                    .position(|f| *f == c.name)
                    .expect(&format!("no column named '{}'", c.name));
                data[ri][idx] = DataType::from(row.get(ci).unwrap());
            }
        }

        match putter.multi_put(data) {
            Ok(_) => {
                // XXX(malte): last_insert_id needs to be set correctly
                // Could we have put more than one row?
                results.completed(q.data.len() as u64, last_insert_id)
            }
            Err(e) => {
                error!(self.log, "put error: {:?}", e);
                results.error(
                    msql_srv::ErrorKind::ER_UNKNOWN_ERROR,
                    format!("{:?}", e).as_bytes(),
                )
            }
        }
    }

    fn handle_select<W: io::Write>(
        &mut self,
        q: nom_sql::SelectStatement,
        results: QueryResultWriter<W>,
    ) -> io::Result<()> {
        let qc = self.query_count
            .fetch_add(1, sync::atomic::Ordering::SeqCst);
        let qname = format!("q_{}", qc);

        // first do a migration to add the query if it doesn't exist already
        match self.soup.extend_recipe(format!("QUERY {}: {};", qname, q)) {
            Ok(_) => {
                let ts_lock = self.table_schemas.lock().unwrap();
                let schema = schema_for_query(&(*ts_lock), &q);

                // create a getter if we don't have one for this query already
                // TODO(malte): may need to make one anyway if the query has changed w.r.t. an
                // earlier one of the same name?
                let getter = self.outputs.entry(qname.clone()).or_insert(
                    self.soup
                        .get_getter(&qname)
                        .expect(&format!("no view named '{}'", qname)),
                );

                // now "execute" the query via a bogokey lookup
                match getter.lookup(&DataType::from(0 as i32), true) {
                    Ok(d) => {
                        let num_rows = d.len();
                        if num_rows > 0 {
                            let mut rw = results.start(schema.as_slice()).unwrap();
                            for mut r in d {
                                // drop bogokey
                                r.pop();
                                for c in r {
                                    match c {
                                        DataType::Int(i) => rw.write_col(i as i32)?,
                                        DataType::BigInt(i) => rw.write_col(i as i64)?,
                                        DataType::Text(t) => rw.write_col(t.to_str().unwrap())?,
                                        dt @ DataType::TinyText(_) => rw.write_col(dt.to_string())?,
                                        _ => unimplemented!(),
                                    }
                                }
                                rw.end_row()?;
                            }
                            rw.finish()
                        } else {
                            results.completed(0, 0)
                        }
                    }
                    Err(_) => {
                        error!(self.log, "error executing SELECT");
                        results.error(
                            msql_srv::ErrorKind::ER_UNKNOWN_ERROR,
                            "Soup returned an error".as_bytes(),
                        )
                    }
                }
            }
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e)),
        }
    }

    fn handle_set<W: io::Write>(
        &mut self,
        _q: nom_sql::SetStatement,
        results: QueryResultWriter<W>,
    ) -> io::Result<()> {
        // ignore
        results.completed(0, 0)
    }

    fn handle_update<W: io::Write>(
        &mut self,
        mut q: nom_sql::UpdateStatement,
        results: QueryResultWriter<W>,
    ) -> io::Result<()> {
        let cond = q.where_clause
            .expect("only supports UPDATEs with WHERE-clauses");

        let ts = self.table_schemas.lock().unwrap();
        let CreateTableStatement { ref fields, .. } = ts[&q.table.name];
        let pkey = utils::get_primary_key(&ts[&q.table.name]);
        let key_indices: Vec<_> = pkey.iter().map(|&(i, _)| i).collect();
        let key_values: Vec<_> = pkey.iter().map(|&(_, c)| c).collect();

        // Updating rows happens in three steps:
        // 1. Read from Soup by key to get full rows
        // 2. Rewrite the column values specified in SET part of UPDATE clause
        // 3. Write results to Soup, deleting old rows, then putting new ones
        //
        // To accomplish the first step we need to buld a getter that retrieves
        // all the columns in the table:
        let where_clause = Some(ConditionExpression::ComparisonOp(ConditionTree {
            operator: Operator::Equal,
            left: box ConditionExpression::Base(ConditionBase::Field(key_values[0].clone())),
            right: box ConditionExpression::Base(ConditionBase::Placeholder),
        }));

        let select_q = SelectStatement {
            tables: vec![q.table.clone()],
            fields: fields
                .into_iter()
                .map(|cs| FieldExpression::Col(cs.column.clone()))
                .collect(),
            distinct: false,
            join: vec![],
            where_clause,
            group_by: None,
            order: None,
            limit: None,
        };

        // update_columns maps column indices to the value they should be updated to:
        let mut update_columns = HashMap::new();
        for (i, field) in fields.into_iter().enumerate() {
            for &mut (ref mut update_column, ref value) in q.fields.iter_mut() {
                // we must ensure that all columns have their table set because the schema we're
                // comparing against sets it; the parser does not itself guarantee this.
                if update_column.table.is_none() {
                    update_column.table = Some(q.table.name.clone());
                }
                if update_column == &field.column {
                    update_columns.insert(i, DataType::from(value));
                }
            }
        }

        // create a mutator if we don't have one for this table already
        let mutator = self.inputs
            .entry(q.table.name.clone())
            .or_insert(self.soup.get_mutator(&q.table.name).unwrap());

        match utils::flatten_conditional(&cond, &key_values) {
            None => results.completed(0, 0),
            Some(ref flattened) if flattened.len() == 0 => {
                panic!("UPDATE only supports WHERE-clauses on primary keys");
            }
            Some(flattened) => {
                let qc = self.query_count
                    .fetch_add(1, sync::atomic::Ordering::SeqCst);
                let qname = format!("q_{}", qc);
                let getter = match self.soup
                    .extend_recipe(format!("QUERY {}: {};", qname, select_q))
                {
                    Ok(_) => self.outputs.entry(qname.clone()).or_insert(
                        self.soup
                            .get_getter(&qname)
                            .expect(&format!("no view named '{}'", qname)),
                    ),
                    Err(e) => {
                        error!(self.log, "update: {:?}", e);
                        return results.error(
                            msql_srv::ErrorKind::ER_UNKNOWN_ERROR,
                            format!("{:?}", e).as_bytes(),
                        );
                    }
                };

                // NOTE: Soup doesn't support compound primary key reads yet,
                // so we'll use only part of the key and filter out what we get later.
                let keys = flattened.iter().map(|key| key[0].clone()).collect();
                let lookup_results = match getter.multi_lookup(keys, true) {
                    Ok(r) => r,
                    Err(e) => {
                        error!(self.log, "update: {:?}", e);
                        return results.error(
                            msql_srv::ErrorKind::ER_UNKNOWN_ERROR,
                            format!("{:?}", e).as_bytes(),
                        );
                    }
                };

                // multi_lookup gives us a vector of results for each key, i.e.:
                // [
                //   [[1, "bob"], [1, "anne"]],
                //   [[2, "cat"], [1, "dog"]],
                // ]
                //
                // We want to turn that into a flat list of records, and update each record with
                // the new values given in the query:
                let changed_rows: Vec<(Vec<DataType>, Vec<DataType>)> = lookup_results
                    .into_iter()
                    .enumerate()
                    .map(|(i, result)| {
                        result
                            .into_iter()
                            .filter_map(|row| {
                                // NOTE: Since Soup doesn't support reads on compound primary keys
                                // yet, we have to filter out rows where only part of the key
                                // matches:
                                if pkey.len() > 1
                                    && key_indices
                                        .iter()
                                        .any(|key_i| row[*key_i] != flattened[i][*key_i])
                                {
                                    return None;
                                }

                                let new_row = row.clone()
                                    .into_iter()
                                    .enumerate()
                                    .map(|(i, column)| {
                                        update_columns
                                            .get(&i)
                                            .and_then(|v| Some(v.clone()))
                                            .unwrap_or(column)
                                    })
                                    .collect::<Vec<DataType>>();

                                // Filter out rows with no actual content changes:
                                if row == new_row {
                                    None
                                } else {
                                    // Keep both the old and new row here in case we mutated the
                                    // primary key (if that's the case we'll need to delete by the
                                    // old key, _not_ the new one):
                                    Some((row, new_row))
                                }
                            })
                            .collect::<Vec<(Vec<DataType>, Vec<DataType>)>>()
                    })
                    .flat_map(|r| r)
                    .collect();

                if changed_rows.len() == 0 {
                    // This might happen if there's no actual changes in content.
                    return results.completed(0, 0);
                }

                // Then we want to delete the old rows:
                let mut new_rows = vec![];
                for (old_row, new_row) in changed_rows.into_iter() {
                    new_rows.push(new_row);
                    let key: Vec<_> = key_indices.iter().map(|i| old_row[*i].clone()).collect();
                    match mutator.delete(key) {
                        Ok(..) => {}
                        Err(e) => {
                            error!(self.log, "update: {:?}", e);
                            return results.error(
                                msql_srv::ErrorKind::ER_UNKNOWN_ERROR,
                                format!("{:?}", e).as_bytes(),
                            );
                        }
                    };
                }

                // And finally, insert the updated rows:
                let count = new_rows.len() as u64;
                match mutator.multi_put(new_rows) {
                    Ok(..) => results.completed(count, 0),
                    Err(e) => {
                        error!(self.log, "update: {:?}", e);
                        results.error(
                            msql_srv::ErrorKind::ER_UNKNOWN_ERROR,
                            format!("{:?}", e).as_bytes(),
                        )
                    }
                }
            }
        }
    }
}

impl<W: io::Write> MysqlShim<W> for SoupBackend {
    fn on_prepare(&mut self, query: &str, info: StatementMetaWriter<W>) -> io::Result<()> {
        debug!(self.log, "prepare: {}", query);

        let query = utils::sanitize_query(query);

        match nom_sql::parse_query(&query) {
            Ok(q) => match q {
                nom_sql::SqlQuery::Select(q) => {
                    // extract parameter columns
                    let ts_lock = self.table_schemas.lock().unwrap();
                    let schema = schema_for_query(&(*ts_lock), &q);

                    // XXX(malte): incorrect! need to extract parameter columns
                    let params: Vec<msql_srv::Column> = vec![];

                    // add the query to Soup
                    let qc = self.query_count
                        .fetch_add(1, sync::atomic::Ordering::SeqCst);
                    let qname = format!("q_{}", qc);
                    match self.soup.extend_recipe(format!("QUERY {}: {};", qname, q)) {
                        Ok(_) => {
                            // register a new prepared statement
                            self.prepared_count += 1;
                            self.prepared.insert(qname, self.prepared_count);
                            info.reply(self.prepared_count, params.as_slice(), schema.as_slice())
                        }
                        Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e)),
                    }
                }
                _ => {
                    // Soup only supports prepared SELECT statements at the moment
                    error!(
                        self.log,
                        "Unsupported query for prepared statement: {}", query
                    );
                    return info.error(
                        msql_srv::ErrorKind::ER_NOT_SUPPORTED_YET,
                        "unsupported query".as_bytes(),
                    );
                }
            },
            Err(e) => {
                // if nom-sql rejects the query, there is no chance Soup will like it
                error!(self.log, "query can't be parsed: \"{}\"", query);
                info.error(msql_srv::ErrorKind::ER_PARSE_ERROR, e.as_bytes())
            }
        }
    }

    fn on_execute(
        &mut self,
        id: u32,
        _: ParamParser,
        results: QueryResultWriter<W>,
    ) -> io::Result<()> {
        error!(self.log, "exec: {}", id);
        results.completed(0, 0)
    }

    fn on_close(&mut self, _: u32) {}

    fn on_query(&mut self, query: &str, results: QueryResultWriter<W>) -> io::Result<()> {
        debug!(self.log, "query: {}", query);

        let query = utils::sanitize_query(query);

        if query.to_lowercase().contains("show databases")
            || query.to_lowercase().starts_with("begin")
            || query.to_lowercase().starts_with("rollback")
            || query.to_lowercase().starts_with("alter table")
            || query.to_lowercase().starts_with("commit")
            || query.to_lowercase().starts_with("create index")
            || query.to_lowercase().starts_with("create unique index")
            || query.to_lowercase().starts_with("create fulltext index")
        {
            warn!(
                self.log,
                "ignoring unsupported query \"{}\" and returning empty results", query
            );
            return results.completed(0, 0);
        }

        if query.to_lowercase().contains("show tables") {
            let cols = [
                Column {
                    table: String::from(""),
                    column: String::from("Tables"),
                    coltype: ColumnType::MYSQL_TYPE_STRING,
                    colflags: ColumnFlags::empty(),
                },
            ];
            let writer = results.start(&cols)?;
            println!(" -> Ok({} rows)", 0);
            return writer.finish();
        }

        for &(ref pattern, ref columns) in &*utils::HARD_CODED_REPLIES {
            if pattern.is_match(&query) {
                let cols: Vec<_> = columns
                    .iter()
                    .map(|c| Column {
                        table: String::from(""),
                        column: String::from(c.0),
                        coltype: ColumnType::MYSQL_TYPE_STRING,
                        colflags: ColumnFlags::empty(),
                    })
                    .collect();
                let mut writer = results.start(&cols[..])?;
                for &(_, ref r) in columns {
                    writer.write_col(String::from(*r))?;
                }
                return writer.end_row();
            }
        }

        match nom_sql::parse_query(&query) {
            Ok(q) => match q {
                nom_sql::SqlQuery::CreateTable(q) => self.handle_create_table(q, results),
                nom_sql::SqlQuery::Insert(q) => self.handle_insert(q, results),
                nom_sql::SqlQuery::Select(q) => self.handle_select(q, results),
                nom_sql::SqlQuery::Set(q) => self.handle_set(q, results),
                nom_sql::SqlQuery::Update(q) => self.handle_update(q, results),
                nom_sql::SqlQuery::Delete(q) => self.handle_delete(q, results),
                nom_sql::SqlQuery::DropTable(q) => {
                    warn!(self.log, "Ignoring DROP TABLE query: \"{}\"", q)
                }
                _ => {
                    error!(self.log, "Unsupported query: {}", query);
                    return results.error(
                        msql_srv::ErrorKind::ER_NOT_SUPPORTED_YET,
                        "unsupported query".as_bytes(),
                    );
                }
            },
            Err(_e) => {
                // if nom-sql rejects the query, there is no chance Soup will like it
                error!(self.log, "query can't be parsed: \"{}\"", query);
                results.completed(0, 0)
                //return results.error(msql_srv::ErrorKind::ER_PARSE_ERROR, e.as_bytes());
            }
        }
    }
}
