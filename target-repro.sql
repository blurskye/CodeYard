\set ON_ERROR_STOP 1
INSERT INTO directory_cells(id,name,region,state,web_urls,ssh_addresses,rpc_addresses,database_bytes,write_rate,maximum_database_bytes,maximum_write_rate) VALUES(0,'a','a','open','["http://127.0.0.1"]','[]','[]',0,0,100,100);
INSERT INTO directory_objects(kind,id,home_cell,nonce_hash,expires_at) VALUES('user',1,0,repeat('e',64),clock_timestamp()+INTERVAL '1 hour');
INSERT INTO directory_claims(shard,kind,key_hash,value,object_kind,object_id,nonce_hash,expires_at) VALUES(0,'name',decode(repeat('ab',32),'hex'),'interrupted','user',1,repeat('e',64),clock_timestamp()+INTERVAL '1 hour');
UPDATE directory_objects SET expires_at=clock_timestamp()-INTERVAL '1 second' WHERE id=1;
SELECT claim_count FROM directory_objects;
BEGIN;
DELETE FROM directory_objects WHERE (kind,id) IN (SELECT kind,id FROM directory_objects WHERE NOT active AND expires_at<=clock_timestamp() ORDER BY expires_at,kind,id LIMIT 100 FOR UPDATE SKIP LOCKED);
COMMIT;
SELECT 'objects', count(*) FROM directory_objects;
SELECT 'claims', count(*) FROM directory_claims;
