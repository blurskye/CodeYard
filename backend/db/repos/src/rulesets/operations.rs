use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_db_core::queries::rulesets::records::ListFilter;
use codeyard_db_core::queries::rulesets::records::Owner;
use codeyard_db_core::queries::rulesets::records::Page;
use codeyard_db_core::queries::rulesets::records::Ruleset;
use codeyard_db_core::queries::rulesets::records::Suite;
use codeyard_db_core::queries::rulesets::records::SuiteFilter;
use codeyard_db_core::queries::rulesets::records::Version;
use codeyard_db_core::queries::rulesets::records::View;
use codeyard_db_core::queries::rulesets::Definition;
use codeyard_db_core::DbPool;
use serde_json::Value;

native_operation!(codeyard_db_orgs::postgres::organizations::rulesets::list,codeyard_db_orgs::sqlite::organizations::rulesets::list,
    fn list_rulesets(pool:&DbPool,actor:Option<i64>,owner:Owner,filter:&ListFilter,page:Page)->Result<OrganizationOutcome<Vec<View<Ruleset>>>>);
native_operation!(codeyard_db_orgs::postgres::organizations::rulesets::get,codeyard_db_orgs::sqlite::organizations::rulesets::get,
    fn get_ruleset(pool:&DbPool,actor:Option<i64>,owner:Owner,id:i64,includes_parents:bool)->Result<OrganizationOutcome<View<Ruleset>>>);
native_operation!(codeyard_db_orgs::postgres::organizations::rulesets::create,codeyard_db_orgs::sqlite::organizations::rulesets::create,
    fn create_ruleset(pool:&DbPool,actor:i64,owner:Owner,input:&Definition)->Result<OrganizationOutcome<Ruleset>>);
native_operation!(codeyard_db_orgs::postgres::organizations::rulesets::change,codeyard_db_orgs::sqlite::organizations::rulesets::change,
    fn change_ruleset(pool:&DbPool,actor:i64,owner:Owner,id:i64,input:&Value)->Result<OrganizationOutcome<Ruleset>>);
native_operation!(codeyard_db_orgs::postgres::organizations::rulesets::remove,codeyard_db_orgs::sqlite::organizations::rulesets::remove,
    fn remove_ruleset(pool:&DbPool,actor:i64,owner:Owner,id:i64)->Result<OrganizationOutcome<()>>);
native_operation!(codeyard_db_orgs::postgres::organizations::rulesets::versions,codeyard_db_orgs::sqlite::organizations::rulesets::versions,
    fn ruleset_versions(pool:&DbPool,actor:Option<i64>,owner:Owner,id:i64,page:Page)->Result<OrganizationOutcome<View<Vec<Version>>>>);
native_operation!(codeyard_db_orgs::postgres::organizations::rulesets::version,codeyard_db_orgs::sqlite::organizations::rulesets::version,
    fn ruleset_version(pool:&DbPool,actor:Option<i64>,owner:Owner,id:i64,version:i64)->Result<OrganizationOutcome<View<Version>>>);
native_operation!(codeyard_db_orgs::postgres::organizations::rulesets::suites::list_suites,codeyard_db_orgs::sqlite::organizations::rulesets::suites::list_suites,
    fn organization_rule_suites(pool:&DbPool,actor:i64,org:i64,filter:&SuiteFilter,page:Page)->Result<OrganizationOutcome<Vec<Suite>>>);
native_operation!(codeyard_db_orgs::postgres::organizations::rulesets::suites::get_suite,codeyard_db_orgs::sqlite::organizations::rulesets::suites::get_suite,
    fn organization_rule_suite(pool:&DbPool,actor:i64,org:i64,id:i64)->Result<OrganizationOutcome<Suite>>);
native_operation!(codeyard_db_orgs::postgres::organizations::rulesets::push_rulesets,codeyard_db_orgs::sqlite::organizations::rulesets::push_rulesets,
    fn push_rulesets(pool:&DbPool,repo:i64)->Result<Vec<Ruleset>>);
native_operation!(codeyard_db_orgs::postgres::organizations::rulesets::bypass_teams,codeyard_db_orgs::sqlite::organizations::rulesets::bypass_teams,
    fn bypass_teams(pool:&DbPool,repo:i64,actor:i64,teams:&[i64])->Result<Vec<i64>>);
native_operation!(codeyard_db_orgs::postgres::organizations::rulesets::suites::record_suite,codeyard_db_orgs::sqlite::organizations::rulesets::suites::record_suite,
    fn record_rule_suite(pool:&DbPool,suite:&Suite)->Result<i64>);
native_operation!(codeyard_db_orgs::postgres::organizations::rulesets::suites::list_repository_suites,codeyard_db_orgs::sqlite::organizations::rulesets::suites::list_repository_suites,
    fn repository_rule_suites(pool:&DbPool,actor:i64,repo:i64,filter:&SuiteFilter,page:Page)->Result<OrganizationOutcome<Vec<Suite>>>);
native_operation!(codeyard_db_orgs::postgres::organizations::rulesets::suites::get_repository_suite,codeyard_db_orgs::sqlite::organizations::rulesets::suites::get_repository_suite,
    fn repository_rule_suite(pool:&DbPool,actor:i64,repo:i64,id:i64)->Result<OrganizationOutcome<Suite>>);
