[out:json][timeout:90][date:"2026-11-22T12:24:51Z"][bbox:46.346,14.134,46.373,14.173];
way["waterway"="river"]["name"="Sava Dolinka"]->.sava;
(
  .sava;
  way(around.sava:48)["highway"]["bridge"];
  way(around.sava:48)["railway"]["bridge"];
);
out body geom;
