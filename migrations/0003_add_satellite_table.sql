-- Add migration script here
CREATE TABLE satellites (
	id SERIAL PRIMARY KEY,
	name TEXT NOT NULL,
	bom_satellite_id TEXT NOT NULL,
	created_at TIMESTAMP WITHOUT TIME ZONE DEFAULT now()
);

INSERT INTO satellites (name, bom_satellite_id) VALUES ('Himawari 9', 'IDE00416');
