@description('Where to create the resources.')
param location string = resourceGroup().location

@description('The tag of ghcr.io/eggspot/eggdb to run, such as 0.3.0.')
param imageTag string = 'latest'

@secure()
@description('The password of the eggdb user.')
param password string

@description('The storage account that holds the database file.')
param storageAccountName string = 'eggdb${uniqueString(resourceGroup().id)}'

resource storage 'Microsoft.Storage/storageAccounts@2023-01-01' = {
  name: storageAccountName
  location: location
  sku: {
    name: 'Premium_LRS'
  }
  kind: 'FileStorage'
  properties: {
    minimumTlsVersion: 'TLS1_2'
    allowBlobPublicAccess: false
    supportsHttpsTrafficOnly: true
  }
}

resource share 'Microsoft.Storage/storageAccounts/fileServices/shares@2023-01-01' = {
  name: '${storage.name}/default/eggdb'
  properties: {
    shareQuota: 100
  }
}

resource eggdb 'Microsoft.ContainerInstance/containerGroups@2023-05-01' = {
  name: 'eggdb'
  location: location
  dependsOn: [
    share
  ]
  properties: {
    osType: 'Linux'
    restartPolicy: 'Always'
    ipAddress: {
      type: 'Public'
      ports: [
        {
          protocol: 'TCP'
          port: 5432
        }
      ]
    }
    containers: [
      {
        name: 'eggdb'
        properties: {
          image: 'ghcr.io/eggspot/eggdb:${imageTag}'
          ports: [
            {
              port: 5432
            }
          ]
          resources: {
            requests: {
              cpu: 1
              memoryInGB: 2
            }
          }
          volumeMounts: [
            {
              name: 'data'
              mountPath: '/data'
            }
            {
              name: 'secrets'
              mountPath: '/secrets'
              readOnly: true
            }
          ]
        }
      }
    ]
    volumes: [
      {
        name: 'data'
        azureFile: {
          shareName: 'eggdb'
          storageAccountName: storage.name
          storageAccountKey: storage.listKeys().keys[0].value
        }
      }
      {
        name: 'secrets'
        secret: {
          password: base64(password)
        }
      }
    ]
  }
}

@description('Connect with psql "host=<this> port=5432 user=eggdb dbname=eggdb sslmode=require".')
output address string = eggdb.properties.ipAddress.ip
