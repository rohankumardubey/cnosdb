--#DATABASE=tc_between
--#SLEEP=100
--#SORT=true
DROP DATABASE IF EXISTS tc_between;
CREATE DATABASE tc_between WITH TTL '100000d';

CREATE TABLE IF NOT EXISTS m2(f0 BIGINT UNSIGNED , TAGS(t0, t1));

INSERT m2(TIME, t0, f0, t1) VALUES(CAST (1672301798050000000 AS TIMESTAMP), 'Ig.UZ', 531136669299148225, 'n꓃DH~B '), (CAST (1672301798060000000 AS TIMESTAMP), '263356943', 1040920791041719924, ''), (CAST (1672301798070000000 AS TIMESTAMP), '1040920791041719924', 442061994865016078, 'gc.');
INSERT m2(TIME, t0, f0, t1) VALUES(CAST (3031647407609562138 AS TIMESTAMP), 'ᵵh', 4166390262642105876, '7ua'), (CAST (1079616064603730664 AS TIMESTAMP), '}\', 7806435932478031652, 'qy'), (CAST (263356943 AS TIMESTAMP), '0.6287658423307444', 5466573340614276155, ',J씟\h'), (CAST (1742494251700243812 AS TIMESTAMP), '#f^Kr잿z', 196790207, 'aF');
INSERT m2(TIME, t0, f0, t1) VALUES(CAST (3584132160280509277 AS TIMESTAMP), '', 4132058214182166915, 'V*1lE/');

SELECT m2.f0 FROM m2 WHERE CAST(0 AS STRING) BETWEEN (CAST( starts_with(m2.t0, m2.t1) AS STRING)) AND (m2.t1);

SELECT * FROM m2 ORDER BY t1;
SELECT * FROM m2 WHERE t1 <= '0';
SELECT * FROM m2 WHERE t1 >= '0';
SELECT * FROM m2 WHERE t1 <= '8';
SELECT * FROM m2 WHERE t1 BETWEEN '7ua' AND 'aF';
SELECT * FROM m2 WHERE t1 <= 'V*1lE/';
SELECT * FROM m2 WHERE t1 >= 'gc.';