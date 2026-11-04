use crate::backend::db::organizations::OrganizationOutcome;
use crate::backend::db::rulesets::Definition;
use crate::backend::db::rulesets::ListFilter;
use crate::backend::db::rulesets::Owner;
use crate::backend::db::rulesets::Page;
use crate::backend::db::rulesets::Ruleset;
use crate::backend::db::rulesets::Suite;
use crate::backend::db::rulesets::SuiteFilter;
use crate::backend::db::rulesets::Version;
use crate::backend::db::rulesets::View;
use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use serde_json::Value;

native_operation!(crate::backend::db::postgres::organizations::rulesets::list,crate::backend::db::sqlite::organizations::rulesets::list,
    fn list_rulesets(pool:&DbPool,actor:Option<i64>,owner:Owner,filter:&ListFilter,page:Page)->Result<OrganizationOutcome<Vec<View<Ruleset>>>>);
native_operation!(crate::backend::db::postgres::organizations::rulesets::get,crate::backend::db::sqlite::organizations::rulesets::get,
    fn get_ruleset(pool:&DbPool,actor:Option<i64>,owner:Owner,id:i64,includes_parents:bool)->Result<OrganizationOutcome<View<Ruleset>>>);
native_operation!(crate::backend::db::postgres::organizations::rulesets::create,crate::backend::db::sqlite::organizations::rulesets::create,
    fn create_ruleset(pool:&DbPool,actor:i64,owner:Owner,input:&Definition)->Result<OrganizationOutcome<Ruleset>>);
native_operation!(crate::backend::db::postgres::organizations::rulesets::change,crate::backend::db::sqlite::organizations::rulesets::change,
    fn change_ruleset(pool:&DbPool,actor:i64,owner:Owner,id:i64,input:&Value)->Result<OrganizationOutcome<Ruleset>>);
native_operation!(crate::backend::db::postgres::organizations::rulesets::remove,crate::backend::db::sqlite::organizations::rulesets::remove,
    fn remove_ruleset(pool:&DbPool,actor:i64,owner:Owner,id:i64)->Result<OrganizationOutcome<()>>);
native_operation!(crate::backend::db::postgres::organizations::rulesets::versions,crate::backend::db::sqlite::organizations::rulesets::versions,
    fn ruleset_versions(pool:&DbPool,actor:Option<i64>,owner:Owner,id:i64,page:Page)->Result<OrganizationOutcome<View<Vec<Version>>>>);
native_operation!(crate::backend::db::postgres::organizations::rulesets::version,crate::backend::db::sqlite::organizations::rulesets::version,
    fn ruleset_version(pool:&DbPool,actor:Option<i64>,owner:Owner,id:i64,version:i64)->Result<OrganizationOutcome<View<Version>>>);
native_operation!(crate::backend::db::postgres::organizations::rulesets::list_suites,crate::backend::db::sqlite::organizations::rulesets::list_suites,
    fn organization_rule_suites(pool:&DbPool,actor:i64,org:i64,filter:&SuiteFilter,page:Page)->Result<OrganizationOutcome<Vec<Suite>>>);
native_operation!(crate::backend::db::postgres::organizations::rulesets::get_suite,crate::backend::db::sqlite::organizations::rulesets::get_suite,
    fn organization_rule_suite(pool:&DbPool,actor:i64,org:i64,id:i64)->Result<OrganizationOutcome<Suite>>);
native_operation!(crate::backend::db::postgres::organizations::rulesets::push_rulesets,crate::backend::db::sqlite::organizations::rulesets::push_rulesets,
    fn push_rulesets(pool:&DbPool,repo:i64)->Result<Vec<Ruleset>>);
native_operation!(crate::backend::db::postgres::organizations::rulesets::bypass_teams,crate::backend::db::sqlite::organizations::rulesets::bypass_teams,
    fn bypass_teams(pool:&DbPool,repo:i64,actor:i64,teams:&[i64])->Result<Vec<i64>>);
native_operation!(crate::backend::db::postgres::organizations::rulesets::record_suite,crate::backend::db::sqlite::organizations::rulesets::record_suite,
    fn record_rule_suite(pool:&DbPool,suite:&Suite)->Result<i64>);
native_operation!(crate::backend::db::postgres::organizations::rulesets::list_repository_suites,crate::backend::db::sqlite::organizations::rulesets::list_repository_suites,
    fn repository_rule_suites(pool:&DbPool,actor:i64,repo:i64,filter:&SuiteFilter,page:Page)->Result<OrganizationOutcome<Vec<Suite>>>);
native_operation!(crate::backend::db::postgres::organizations::rulesets::get_repository_suite,crate::backend::db::sqlite::organizations::rulesets::get_repository_suite,
    fn repository_rule_suite(pool:&DbPool,actor:i64,repo:i64,id:i64)->Result<OrganizationOutcome<Suite>>);
