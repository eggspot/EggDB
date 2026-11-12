Pod::Spec.new do |spec|
  spec.name = 'eggdb_flutter'
  spec.version = '0.1.0'
  spec.summary = 'EggDB embedded in Flutter apps.'
  spec.homepage = 'https://github.com/eggspot/EggDB'
  spec.license = { :type => 'MIT OR Apache-2.0' }
  spec.author = 'Eggspot'
  spec.source = { :path => '.' }
  spec.platform = :osx, '11.0'
  spec.dependency 'FlutterMacOS'
  spec.vendored_frameworks = 'EggDB.xcframework'
  spec.user_target_xcconfig = { 'OTHER_LDFLAGS' => '$(inherited) -all_load' }
end
