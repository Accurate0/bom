-- Add migration script here
CREATE TABLE locations (
	id SERIAL PRIMARY KEY,
	name TEXT NOT NULL,
	bom_radar_id TEXT NOT NULL,
	created_at TIMESTAMP WITHOUT TIME ZONE DEFAULT now()
);

INSERT INTO locations (name, bom_radar_id) VALUES ('Perth', 'IDR703');
