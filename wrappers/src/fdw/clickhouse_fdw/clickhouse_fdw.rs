use clickhouse_rs::{types, types::Block, types::SqlType, ClientHandle, Pool};
use pgx::log::PgSqlErrorCode;
use pgx::log::{elog, PgLogLevel};
use std::collections::HashMap;
use tokio::runtime::Runtime;

use supabase_wrappers::{
    create_async_runtime, report_error, Cell, ForeignDataWrapper, Limit, Qual, Row, Sort,
};

fn deparse(quals: &Vec<Qual>, columns: &Vec<String>, options: &HashMap<String, String>) -> String {
    let tgts = columns.join(", ");
    let table = options.get("table").unwrap();
    let sql = if quals.is_empty() {
        format!("select {} from {}", tgts, table)
    } else {
        let cond = quals
            .iter()
            .map(|q| q.deparse())
            .collect::<Vec<String>>()
            .join(" and ");
        format!("select {} from {} where {}", tgts, table, cond)
    };
    sql
}

pub(crate) struct ClickHouseFdw {
    rt: Runtime,
    client: Option<ClientHandle>,
    table: String,
    rowid_col: String,
    scan_blk: Option<Block<types::Complex>>,
    row_idx: usize,
}

impl ClickHouseFdw {
    pub fn new(options: &HashMap<String, String>) -> Self {
        let rt = create_async_runtime();
        let conn_str = options.get("conn_string").unwrap();
        let pool = Pool::new(conn_str.as_str());
        let client = rt.block_on(pool.get_handle()).map_or_else(
            |err| {
                elog(PgLogLevel::ERROR, &format!("connection failed: {}", err));
                None
            },
            |client| Some(client),
        );
        ClickHouseFdw {
            rt,
            client,
            table: "".to_string(),
            rowid_col: "".to_string(),
            scan_blk: None,
            row_idx: 0,
        }
    }
}

impl ForeignDataWrapper for ClickHouseFdw {
    fn get_rel_size(
        &mut self,
        quals: &Vec<Qual>,
        columns: &Vec<String>,
        _sorts: &Vec<Sort>,
        _limit: &Option<Limit>,
        options: &HashMap<String, String>,
    ) -> (i64, i32) {
        if let Some(ref mut client) = self.client {
            self.table = options.get("table").map(|t| t.to_owned()).unwrap();
            self.rowid_col = options.get("rowid_column").map(|r| r.to_owned()).unwrap();

            // for simplicity purpose, we fetch whole query result to local,
            // may need optimization in the future.
            let sql = deparse(quals, columns, options);
            match self.rt.block_on(client.query(&sql).fetch_all()) {
                Ok(block) => {
                    let rows = block.row_count();
                    let width = block.column_count() * 8;
                    self.scan_blk = Some(block);
                    return (rows as i64, width as i32);
                }
                Err(err) => elog(PgLogLevel::ERROR, &format!("query failed: {}", err)),
            }
        }
        (0, 0)
    }

    fn begin_scan(
        &mut self,
        _quals: &Vec<Qual>,
        _columns: &Vec<String>,
        _sorts: &Vec<Sort>,
        _limit: &Option<Limit>,
        _options: &HashMap<String, String>,
    ) {
        self.row_idx = 0;
    }

    fn iter_scan(&mut self) -> Option<Row> {
        if let Some(block) = &self.scan_blk {
            let mut ret = Row::new();
            let mut rows = block.rows();

            if let Some(row) = rows.nth(self.row_idx) {
                for i in 0..block.column_count() {
                    let col_name = row.name(i).unwrap();
                    let sql_type = row.sql_type(i).unwrap();
                    let cell = match sql_type {
                        SqlType::UInt8 => {
                            // Bool is stored as UInt8 in ClickHouse, so we treat it as bool here
                            let value = row.get::<u8, usize>(i).unwrap();
                            Cell::Bool(value != 0)
                        }
                        SqlType::Float64 => {
                            let value = row.get::<f64, usize>(i).unwrap();
                            Cell::F64(value)
                        }
                        SqlType::Int64 => {
                            let value = row.get::<i64, usize>(i).unwrap();
                            Cell::I64(value)
                        }
                        SqlType::String => {
                            let value = row.get::<String, usize>(i).unwrap();
                            Cell::String(value)
                        }
                        _ => {
                            report_error(
                                PgSqlErrorCode::ERRCODE_FDW_INVALID_DATA_TYPE,
                                &format!("data type {} is not supported", sql_type.to_string()),
                            );
                            return None;
                        }
                    };
                    ret.push(col_name, Some(cell));
                }

                self.row_idx += 1;
                return Some(ret);
            }
        }
        None
    }

    fn end_scan(&mut self) {
        self.scan_blk.take();
    }

    fn begin_modify(&mut self, options: &HashMap<String, String>) {
        self.table = options.get("table").map(|t| t.to_owned()).unwrap();
        self.rowid_col = options.get("rowid_column").map(|r| r.to_owned()).unwrap();
    }

    fn insert(&mut self, src: &Row) {
        if let Some(ref mut client) = self.client {
            let mut row = Vec::new();
            for (col_name, cell) in src.iter() {
                let col_name = col_name.to_owned();
                if let Some(cell) = cell {
                    match cell {
                        Cell::Bool(v) => row.push((col_name, types::Value::from(*v))),
                        Cell::F64(v) => row.push((col_name, types::Value::from(*v))),
                        Cell::I64(v) => row.push((col_name, types::Value::from(*v))),
                        Cell::String(v) => row.push((col_name, types::Value::from(v.as_str()))),
                        _ => elog(
                            PgLogLevel::ERROR,
                            &format!("field type {:?} not supported", cell),
                        ),
                    }
                }
            }
            let mut block = Block::new();
            block.push(row).unwrap();

            // execute query on ClickHouse
            if let Err(err) = self.rt.block_on(client.insert(&self.table, block)) {
                elog(PgLogLevel::ERROR, &format!("insert failed: {}", err));
            }
        }
    }

    fn update(&mut self, rowid: &Cell, new_row: &Row) {
        if let Some(ref mut client) = self.client {
            let mut sets = Vec::new();
            for (col, cell) in new_row.iter() {
                if col == &self.rowid_col {
                    continue;
                }
                if let Some(cell) = cell {
                    sets.push(format!("{} = {}", col, cell));
                } else {
                    sets.push(format!("{} = null", col));
                }
            }
            let sql = format!(
                "alter table {} update {} where {} = {}",
                self.table,
                sets.join(", "),
                self.rowid_col,
                rowid
            );

            // execute query on ClickHouse
            if let Err(err) = self.rt.block_on(client.execute(&sql)) {
                elog(PgLogLevel::ERROR, &format!("update failed: {}", err));
            }
        }
    }

    fn end_modify(&mut self) {}

    fn delete(&mut self, rowid: &Cell) {
        if let Some(ref mut client) = self.client {
            let sql = format!(
                "alter table {} delete where {} = {}",
                self.table, self.rowid_col, rowid
            );

            // execute query on ClickHouse
            if let Err(err) = self.rt.block_on(client.execute(&sql)) {
                elog(PgLogLevel::ERROR, &format!("delete failed: {}", err));
            }
        }
    }
}